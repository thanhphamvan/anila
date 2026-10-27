//! Render shared styles and the real xterm without connecting to a machine.

use std::{fs, time::Duration};

use anila_e2e::{browser::Browser, util::workspace_root};
use serde_json::json;

#[test]
fn mobile_terminal_and_forms_fit_the_screen() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let browser = Browser::launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        let root = workspace_root().join("frontend");
        let css = format!("{}\n{}", fs::read_to_string(root.join("vendor/xterm/xterm.css")).unwrap(), fs::read_to_string(root.join("style.css")).unwrap());
        let html = format!(r#"<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1"><style>{css}</style>
            <main class="app"><section class="terminal">
            <nav class="tabbar"><button class="home">⌂</button><div class="tabs">
            <div class="tab active"><button class="tab-open"><span class="tab-name">a-very-long-terminal-session-name</span></button><button class="tab-close">×</button></div>
            <div class="tab"><button class="tab-open">another session</button></div></div><button class="tab-new">+</button></nav>
            <div class="terms-mount"><div class="terms"><div class="term" id="terminal"></div></div></div>
            <nav class="keys"><button>Ctrl</button><button>Esc</button><button>Tab</button><button>↑</button><button>↓</button><button>←</button><button>→</button><button>Enter</button></nav>
            </section></main>"#);
        page.eval(&format!("document.open();document.write({});document.close()", json!(html))).await.unwrap();
        page.emulate_phone().await.unwrap();
        for asset in ["vendor/xterm/xterm.js", "vendor/xterm/addon-fit.js", "viewport.js"] {
            page.eval(&fs::read_to_string(root.join(asset)).unwrap()).await.unwrap();
        }
        page.eval("const terminal = new Terminal({fontSize:14}); const fit = new FitAddon.FitAddon(); terminal.loadAddon(fit); terminal.open(document.querySelector('#terminal')); window.addEventListener('resize', () => fit.fit()); fit.fit(); terminal.write('Mobile terminal ready\\r\\n' + 'x'.repeat(200))").await.unwrap();
        for (width, height, mobile) in [(320,640,true), (390,844,true), (390,300,true), (844,390,true), (768,1024,true), (1280,800,false)] {
            page.set_viewport(width, height, mobile).await.unwrap();
            page.wait_for(&format!("Math.abs(document.querySelector('.app').getBoundingClientRect().height - {height}) <= 1"), Duration::from_secs(5)).await.unwrap();
            let layout = page.eval(r#"(() => {
                const app = document.querySelector('.app').getBoundingClientRect();
                const keys = document.querySelector('.keys').getBoundingClientRect();
                const term = document.querySelector('.term').getBoundingClientRect();
                return { width:app.width, height:app.height, keysBottom:keys.bottom, termHeight:term.height, cols:terminal.cols, rows:terminal.rows, overflow:document.documentElement.scrollWidth > innerWidth, home:document.querySelector('.home').getBoundingClientRect().width };
            })()"#).await.unwrap();
            assert_eq!(layout["overflow"], false, "{width}x{height}: {layout}");
            assert!(layout["keysBottom"].as_f64().unwrap() <= f64::from(height) + 1.0, "{layout}");
            assert!(layout["cols"].as_u64().unwrap() >= 20 && layout["rows"].as_u64().unwrap() >= 3, "{layout}");
            if mobile { assert!(layout["home"].as_f64().unwrap() >= 44.0, "{layout}"); }
        }
        page.set_viewport(320, 480, true).await.unwrap();
        page.eval(r#"document.querySelector('.app').innerHTML = '<section class="sessions"><div class="bar"><span class="host">a-very-long-hostname</span><button>Account settings</button></div><button class="row"><span class="name">' + 'long-session-name-'.repeat(20) + '</span><span class="badge">Shared session</span><span class="meta">' + 'long-description-'.repeat(20) + '</span></button><form><label>Session name<input></label><button>Create session</button></form></section>'"#).await.unwrap();
        let form = page.eval("({overflow:document.documentElement.scrollWidth > innerWidth, font:getComputedStyle(document.querySelector('input')).fontSize, scroll:document.querySelector('.sessions').scrollHeight > document.querySelector('.sessions').clientHeight})").await.unwrap();
        assert_eq!(form["overflow"], false, "{form}");
        assert_eq!(form["font"], "16px", "mobile inputs must avoid automatic zoom");
        assert_eq!(form["scroll"], true, "long content must scroll within the screen");
    });
}
