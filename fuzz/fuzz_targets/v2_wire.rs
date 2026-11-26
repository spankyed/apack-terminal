#![no_main]

use apack_proto::frame::{DecodedFrame, FrameDecoder};
use apack_proto::messages::{BrokerMsg, ClientMsg};
use libfuzzer_sys::fuzz_target;

// Fuzzes the v2 binary frame decoder and both client/broker message decoders across arbitrary
// chunk splits (whole buffer and 1-byte streaming), verifying totality and split invariance.
fuzz_target!(|data: &[u8]| {
    let whole = decode_all(data, data.len().max(1));
    let by_one = decode_all(data, 1);
    assert_eq!(whole, by_one);

    if let Ok(frames) = whole {
        for frame in frames {
            if let DecodedFrame::Frame { header, payload } = frame {
                if let Ok(msg) = ClientMsg::decode(header.msg_type, &payload) {
                    let mut w = apack_proto::codec::Writer::new();
                    msg.encode(&mut w);
                    let reparsed = ClientMsg::decode(msg.msg_type(), w.as_slice())
                        .expect("encoded ClientMsg must round-trip");
                    assert_eq!(reparsed, msg);
                }
                if let Ok(msg) = BrokerMsg::decode(header.msg_type, &payload) {
                    let mut w = apack_proto::codec::Writer::new();
                    msg.encode(&mut w);
                    let reparsed = BrokerMsg::decode(msg.msg_type(), w.as_slice())
                        .expect("encoded BrokerMsg must round-trip");
                    assert_eq!(reparsed, msg);
                }
            }
        }
    }
});

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
