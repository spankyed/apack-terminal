#![no_main]

use apack_proto::frame::{DecodedFrame, FrameDecoder};
use apack_proto::messages::{BrokerMsg, ClientMsg};
use libfuzzer_sys::fuzz_target;

// Fuzzes the v2 binary frame decoder across arbitrary chunk splits and directly exercises
// both client and broker message decoders on raw payloads, verifying totality and round-trips.
fuzz_target!(|data: &[u8]| {
    let whole = decode_all(data, data.len().max(1));
    let by_one = decode_all(data, 1);
    assert_eq!(whole, by_one);

    if let Ok(frames) = whole {
        for frame in frames {
            if let DecodedFrame::Frame { header, payload } = frame {
                check_client(header.msg_type, &payload);
                check_broker(header.msg_type, &payload);
            }
        }
    }

    // Directly fuzz message decoders without needing a synchronized 4-byte frame length header.
    if data.len() >= 2 {
        let tag = u16::from_le_bytes([data[0], data[1]]);
        let body = &data[2..];
        check_client(tag, body);
        check_broker(tag, body);
    }
});

fn check_client(tag: u16, payload: &[u8]) {
    if let Ok(msg) = ClientMsg::decode(tag, payload) {
        let mut w = apack_proto::codec::Writer::new();
        msg.encode(&mut w);
        let reparsed = ClientMsg::decode(msg.msg_type(), w.as_slice())
            .expect("encoded ClientMsg must round-trip");
        assert_eq!(reparsed, msg);
    }
}

fn check_broker(tag: u16, payload: &[u8]) {
    if let Ok(msg) = BrokerMsg::decode(tag, payload) {
        let mut w = apack_proto::codec::Writer::new();
        msg.encode(&mut w);
        let reparsed = BrokerMsg::decode(msg.msg_type(), w.as_slice())
            .expect("encoded BrokerMsg must round-trip");
        assert_eq!(reparsed, msg);
    }
}

fn decode_all(data: &[u8], chunk_size: usize) -> Result<Vec<DecodedFrame>, apack_proto::codec::WireError> {
    let mut dec = FrameDecoder::new();
    let mut out = Vec::new();
    for chunk in data.chunks(chunk_size) {
        let mut rest = chunk;
        while !rest.is_empty() {
            let (used, frame) = dec.step(rest)?;
            assert!(used > 0);
            rest = &rest[used..];
            if let Some(f) = frame {
                out.push(f);
            }
        }
    }
    Ok(out)
}
