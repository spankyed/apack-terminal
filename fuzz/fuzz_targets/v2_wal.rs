#![no_main]

use apack_wal::format::{
    RecordingMeta, SegmentFooter, decode_one_record, encode_framed_record, scan_segment,
};
use libfuzzer_sys::fuzz_target;

// Fuzzes every v2 WAL on-disk decoder (`meta.apkm`, `SegmentFooter`, individual framed records,
// and full segment scans with torn tails and zlib keyframe decompression caps).
fuzz_target!(|data: &[u8]| {
    if let Ok(meta) = RecordingMeta::decode(data) {
        let encoded = meta.encode();
        assert_eq!(RecordingMeta::decode(&encoded).expect("meta round-trip"), meta);
    }

    if let Ok(footer) = SegmentFooter::decode(data) {
        let encoded = footer.encode();
        assert_eq!(SegmentFooter::decode(&encoded).expect("footer round-trip"), footer);
    }

    if let Ok((timed, consumed)) = decode_one_record(data) {
        assert!(consumed <= data.len());
        let mut reencoded = Vec::new();
        if encode_framed_record(timed.at_ms, &timed.record, &mut reencoded).is_ok() {
            let (reparsed, re_consumed) =
                decode_one_record(&reencoded).expect("encoded WAL record must round-trip");
            assert_eq!(re_consumed, reencoded.len());
            assert_eq!(reparsed, timed);
        }
    }

    let scan = scan_segment(data);
    assert!(scan.valid_records_end <= data.len());
});
