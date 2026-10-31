use super::BoundedSink;
use std::io::Write as _;

#[test]
fn capped_writer_bounds_capacity_across_growth_and_rejects_overflow() {
    let envelope = "sink exceeds its envelope";

    // Capacity stays within the smaller of the byte limit and the cap.
    for (limit, capacity_cap) in [(31, u64::MAX), (u64::MAX, 31)] {
        {
            let mut bytes = Vec::new();
            let mut sink = BoundedSink::capture_capped(&mut bytes, limit, capacity_cap, envelope);

            let mut growths = 0;
            for size in [1, 1, 2, 4, 8, 1, 5, 1, 8] {
                let previous = sink.capacity();
                sink.write_all(&vec![b'x'; size]).unwrap();
                growths += usize::from(sink.capacity() > previous);
                assert!(sink.capacity() <= 31);
            }
            assert!(growths <= 6, "writes should grow geometrically");
            assert_eq!(sink.captured_len(), 31);
            assert!(sink.write(b"x").unwrap_err().to_string().contains(envelope));
            assert_eq!(sink.captured_len(), 31);
            assert!(sink.capacity() <= 31);
        }

        {
            let mut bytes = Vec::new();
            let mut oversized =
                BoundedSink::capture_capped(&mut bytes, limit, capacity_cap, envelope);
            assert!(oversized.write(&[0; 32]).is_err());
            assert_eq!(oversized.capacity(), 0);
        }

        // Also reject an already overallocated Vec without attempting to
        // shrink it or copying any bytes into it.
        let mut bytes = Vec::with_capacity(32);
        {
            let mut overallocated =
                BoundedSink::capture_capped(&mut bytes, limit, capacity_cap, envelope);
            assert!(overallocated.write(b"x").is_err());
        }
        assert!(bytes.is_empty());
    }
}
