//! Golden frames in `proto/vectors/`. Other crates' tests decode the same
//! files, so a contract change shows up as a failing vector everywhere.

use anila_proto::{Data, Hop, ToAgent, ToBrowser};
use serde_json::Value;

const CONTROL: &str = include_str!("../vectors/control.json");
const DATA: &str = include_str!("../vectors/data.json");

fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(frame: &Value) -> Value {
    let typed: T = serde_json::from_value(frame.clone()).expect("vector decodes");
    serde_json::to_value(typed).unwrap()
}

#[test]
fn control_vectors_round_trip() {
    let vectors: Vec<Value> = serde_json::from_str(CONTROL).unwrap();
    assert!(!vectors.is_empty());
    for v in vectors {
        let name = v["name"].as_str().unwrap();
        let frame = &v["frame"];
        let back = match v["kind"].as_str().unwrap() {
            "hop" => round_trip::<Hop>(frame),
            "to_agent" => round_trip::<ToAgent>(frame),
            "to_browser" => round_trip::<ToBrowser>(frame),
            other => panic!("{name}: unknown kind {other}"),
        };
        assert_eq!(&back, frame, "vector {name:?} changed on round trip");
    }
}

#[test]
fn control_vectors_classify() {
    let vectors: Vec<Value> = serde_json::from_str(CONTROL).unwrap();
    for v in vectors {
        let route = anila_proto::classify(&v["frame"].to_string());
        let expected = match v["kind"].as_str().unwrap() {
            "hop" => Some(anila_proto::Route::Hop),
            _ => Some(anila_proto::Route::Routed(
                v["frame"]["tab"].as_u64().unwrap() as u32,
            )),
        };
        assert_eq!(route, expected, "vector {:?}", v["name"]);
    }
}

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn data_vectors_decode_and_encode() {
    let vectors: Vec<Value> = serde_json::from_str(DATA).unwrap();
    for v in vectors {
        let name = v["name"].as_str().unwrap();
        let wire = unhex(v["hex"].as_str().unwrap());
        let f = &v["frame"];
        let bytes = unhex(f["bytes_hex"].as_str().unwrap());
        let expected = match f["kind"].as_str().unwrap() {
            "out" => Data::Out {
                tab: f["tab"].as_u64().unwrap() as u32,
                ch: f["ch"].as_u64().unwrap() as u16,
                epoch: f["epoch"].as_u64().unwrap() as u32,
                off: f["off"].as_u64().unwrap(),
                compressed: f["compressed"].as_bool().unwrap(),
                bytes: &bytes,
            },
            "in" => Data::In {
                tab: f["tab"].as_u64().unwrap() as u32,
                ch: f["ch"].as_u64().unwrap() as u16,
                id: f["id"].as_u64().unwrap() as u32,
                bytes: &bytes,
            },
            other => panic!("{name}: unknown kind {other}"),
        };
        assert_eq!(
            Data::decode(&wire).unwrap(),
            expected,
            "vector {name:?} decode"
        );
        assert_eq!(expected.to_vec().unwrap(), wire, "vector {name:?} encode");
    }
}
