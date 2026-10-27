//! Browser transport for the shared client state machine.

use crate::api::error;
use anila_proto::{
    Ch, Create, HostId, Req, SessionName,
    client::{Client, ClientConfig, CloseAction, Event, Frame, backoff_ms},
};
use std::{cell::RefCell, rc::Rc};
use wasm_bindgen::{JsCast, prelude::*};
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// Transport notifications alongside protocol events.
pub enum Notice {
    Protocol(Event),
    Down,
    Stopped(u16, String),
    Error(String),
}

struct State {
    client: Client,
    socket: Option<WebSocket>,
    stopped: bool,
    attempt: u32,
    timer: Option<i32>,
    interval: Option<i32>,
}

/// A connection with explicitly owned browser callbacks and timers.
#[derive(Clone)]
pub struct Connection(Rc<Inner>);
struct Inner {
    host: HostId,
    state: RefCell<State>,
    notify: Box<dyn Fn(Notice)>,
    open: Closure<dyn FnMut(web_sys::Event)>,
    message: Closure<dyn FnMut(MessageEvent)>,
    close: Closure<dyn FnMut(CloseEvent)>,
    tick: Closure<dyn FnMut()>,
    retry: Closure<dyn FnMut()>,
    wake: Closure<dyn FnMut(web_sys::Event)>,
}

fn now() -> u64 {
    js_sys::Date::now() as u64
}

impl Connection {
    /// Install the clock and wake listeners, then connect to this origin.
    pub fn new(host: HostId, notify: impl Fn(Notice) + 'static) -> Result<Self, String> {
        let tab = crate::tab::tab_id()?;
        let inner = Rc::new_cyclic(|weak: &std::rc::Weak<Inner>| {
            let w = weak.clone();
            let open = Closure::new(move |_: web_sys::Event| {
                if let Some(i) = w.upgrade() {
                    let frames = i.state.borrow_mut().client.on_open(now());
                    i.send(frames);
                }
            });
            let w = weak.clone();
            let message = Closure::new(move |e: MessageEvent| {
                if let Some(i) = w.upgrade() {
                    let data = e.data();
                    let result = if let Some(text) = data.as_string() {
                        i.state.borrow_mut().client.on_text(&text, now())
                    } else if data.is_instance_of::<js_sys::ArrayBuffer>() {
                        i.state
                            .borrow_mut()
                            .client
                            .on_binary(&js_sys::Uint8Array::new(&data).to_vec(), now())
                    } else {
                        return;
                    };
                    i.deliver(result);
                }
            });
            let w = weak.clone();
            let close = Closure::new(move |e: CloseEvent| {
                if let Some(i) = w.upgrade() {
                    i.detach();
                    let action = i.state.borrow_mut().client.on_close(e.code());
                    (i.notify)(Notice::Down);
                    match action {
                        CloseAction::Reconnect => i.schedule(),
                        CloseAction::Stop { code, reason } => {
                            i.state.borrow_mut().stopped = true;
                            let reason = if code == 4403 && !e.reason().is_empty() {
                                e.reason()
                            } else {
                                reason
                            };
                            (i.notify)(Notice::Stopped(code, reason));
                        }
                    }
                }
            });
            let w = weak.clone();
            let tick = Closure::new(move || {
                if let Some(i) = w.upgrade() {
                    let result = i.state.borrow_mut().client.tick(now());
                    i.deliver(result);
                }
            });
            let w = weak.clone();
            let retry = Closure::new(move || {
                if let Some(i) = w.upgrade() {
                    i.state.borrow_mut().timer = None;
                    i.connect();
                }
            });
            let w = weak.clone();
            let wake = Closure::new(move |e: web_sys::Event| {
                if let Some(i) = w.upgrade() {
                    let visible = web_sys::window()
                        .and_then(|w| w.document())
                        .is_some_and(|d| d.visibility_state() == web_sys::VisibilityState::Visible);
                    if (e.type_() == "online" || visible)
                        && !i
                            .state
                            .borrow()
                            .socket
                            .as_ref()
                            .is_some_and(|s| s.ready_state() == WebSocket::OPEN)
                    {
                        i.immediate();
                    }
                }
            });
            Inner {
                host,
                state: RefCell::new(State {
                    client: Client::new(tab, ClientConfig::default()),
                    socket: None,
                    stopped: false,
                    attempt: 0,
                    timer: None,
                    interval: None,
                }),
                notify: Box::new(notify),
                open,
                message,
                close,
                tick,
                retry,
                wake,
            }
        });
        let window = web_sys::window().ok_or("Browser window unavailable")?;
        inner.state.borrow_mut().interval = Some(
            window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    inner.tick.as_ref().unchecked_ref(),
                    100,
                )
                .map_err(error)?,
        );
        window
            .add_event_listener_with_callback("online", inner.wake.as_ref().unchecked_ref())
            .map_err(error)?;
        window
            .document()
            .ok_or("Document unavailable")?
            .add_event_listener_with_callback(
                "visibilitychange",
                inner.wake.as_ref().unchecked_ref(),
            )
            .map_err(error)?;
        inner.connect();
        Ok(Self(inner))
    }
    /// Request a fresh session list.
    pub fn list(&self) {
        let frame = self.0.state.borrow_mut().client.list().1;
        self.0.send(frame);
    }
    /// Allocate a channel using the fitted terminal size.
    pub fn open(
        &self,
        name: SessionName,
        create: Option<Create>,
        cols: u16,
        rows: u16,
    ) -> (Ch, Req) {
        let (ch, req, frame) = self
            .0
            .state
            .borrow_mut()
            .client
            .open(name, create, cols, rows);
        self.0.send(frame);
        (ch, req)
    }
    /// Send input; false means the state machine paused it.
    pub fn input(&self, ch: Ch, bytes: &[u8]) -> bool {
        let frames = self.0.state.borrow_mut().client.input(ch, bytes);
        let sent = !frames.is_empty();
        self.0.send(frames);
        sent
    }
    /// Save and send a changed terminal size.
    pub fn resize(&self, ch: Ch, cols: u16, rows: u16) {
        let frame = self.0.state.borrow_mut().client.resize(ch, cols, rows);
        self.0.send(frame);
    }
    /// Detach a terminal channel.
    pub fn close_channel(&self, ch: Ch) {
        let frame = self.0.state.borrow_mut().client.close_channel(ch);
        self.0.send(frame);
    }
}

impl Inner {
    fn detach(&self) {
        if let Some(socket) = self.state.borrow_mut().socket.take() {
            socket.set_onopen(None);
            socket.set_onmessage(None);
            socket.set_onclose(None);
            if let Err(e) = socket.close() {
                (self.notify)(Notice::Error(error(e)));
            }
        }
    }
    fn connect(&self) {
        if self.state.borrow().stopped {
            return;
        }
        let result = (|| {
            let window = web_sys::window().ok_or("Browser window unavailable")?;
            let location =
                js_sys::Reflect::get(window.as_ref(), &"location".into()).map_err(error)?;
            let read = |key: &str| {
                js_sys::Reflect::get(&location, &key.into())
                    .map_err(error)?
                    .as_string()
                    .ok_or_else(|| "Location unavailable".to_string())
            };
            let scheme = if read("protocol")? == "https:" {
                "wss"
            } else {
                "ws"
            };
            WebSocket::new(&format!(
                "{scheme}://{}/ws/term?host={}",
                read("host")?,
                js_sys::encode_uri_component(self.host.as_str())
            ))
            .map_err(error)
        })();
        match result {
            Ok(socket) => {
                socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
                socket.set_onopen(Some(self.open.as_ref().unchecked_ref()));
                socket.set_onmessage(Some(self.message.as_ref().unchecked_ref()));
                socket.set_onclose(Some(self.close.as_ref().unchecked_ref()));
                self.state.borrow_mut().socket = Some(socket);
            }
            Err(e) => {
                (self.notify)(Notice::Error(e));
                self.schedule();
            }
        }
    }
    fn schedule(&self) {
        let mut state = self.state.borrow_mut();
        if state.stopped || state.timer.is_some() {
            return;
        }
        let delay = backoff_ms(state.attempt, js_sys::Math::random());
        state.attempt = state.attempt.saturating_add(1);
        let result = web_sys::window()
            .ok_or("Browser window unavailable".into())
            .and_then(|w| {
                w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    self.retry.as_ref().unchecked_ref(),
                    delay as i32,
                )
                .map_err(error)
            });
        match result {
            Ok(id) => state.timer = Some(id),
            Err(e) => {
                drop(state);
                (self.notify)(Notice::Error(e));
            }
        }
    }
    fn immediate(&self) {
        if self.state.borrow().stopped {
            return;
        }
        if let Some(id) = self.state.borrow_mut().timer.take()
            && let Some(w) = web_sys::window()
        {
            w.clear_timeout_with_handle(id);
        }
        self.detach();
        {
            let mut state = self.state.borrow_mut();
            state.attempt = 0;
            state.client.on_close(1006);
        }
        (self.notify)(Notice::Down);
        self.connect();
    }
    fn send(&self, frames: impl IntoIterator<Item = Frame>) {
        let socket = self.state.borrow().socket.clone();
        if let Some(socket) = socket.filter(|s| s.ready_state() == WebSocket::OPEN) {
            for frame in frames {
                let result = match frame {
                    Frame::Text(s) => socket.send_with_str(&s),
                    Frame::Binary(b) => socket.send_with_u8_array(&b),
                };
                if let Err(e) = result {
                    (self.notify)(Notice::Error(error(e)));
                    self.immediate();
                    break;
                }
            }
        }
    }
    fn deliver(&self, (frames, events): (Vec<Frame>, Vec<Event>)) {
        self.send(frames);
        for event in events {
            if matches!(event, Event::Connected { .. }) {
                self.state.borrow_mut().attempt = 0;
            }
            let dead = matches!(event, Event::LinkDead);
            (self.notify)(Notice::Protocol(event));
            if dead {
                self.immediate();
            }
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.detach();
        if let Some(w) = web_sys::window() {
            let state = self.state.borrow();
            if let Some(id) = state.timer {
                w.clear_timeout_with_handle(id);
            }
            if let Some(id) = state.interval {
                w.clear_interval_with_handle(id);
            }
            let _ =
                w.remove_event_listener_with_callback("online", self.wake.as_ref().unchecked_ref());
            if let Some(d) = w.document() {
                let _ = d.remove_event_listener_with_callback(
                    "visibilitychange",
                    self.wake.as_ref().unchecked_ref(),
                );
            }
        }
    }
}
