use crate::input::{after_header, lines};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, fields, price, timestamp};
use crate::pipeline::{CompactEvent, EventPayload};

#[test]
fn csv_preserves_bytes_and_removes_only_outer_quotes() {
    assert_eq!(
        fields::<3>(b"\" u\",p\r,\"\""),
        Some([b" u".as_slice(), b"p\r", b""])
    );
    assert_eq!(fields::<2>(b","), Some([b"".as_slice(), b"".as_slice()]));
    assert_eq!(fields::<2>(b"a,\"\t\""), Some([b"a".as_slice(), b"\t"]));
}

#[test]
fn csv_rejects_misplaced_quotes_and_wrong_arity() {
    for s in [
        b"".as_slice(),
        b"a",
        b"a,b,c",
        b"\"a,b",
        b"a,b\"",
        b"a,\"b\"\r",
        b"\"a\"\"b\",c",
        b"a,\"\"\"",
        b"a,b\"c",
        b"a, \"b\"",
    ] {
        assert!(fields::<2>(s).is_none(), "{s:?}");
    }
}

#[test]
fn only_lf_ends_a_line_and_only_first_line_is_a_header() {
    let bytes = b"a,b\na,b\n\n1,2\r\nlast,x";
    assert_eq!(
        lines(after_header(bytes)).collect::<Vec<_>>(),
        [b"a,b".as_slice(), b"", b"1,2\r", b"last,x"]
    );
    assert_eq!(lines(after_header(b"a,b")).count(), 0);
}

#[test]
fn timestamps_convert_epoch_and_offsets() {
    let instant = timestamp(b"2026-10-03T23:58:10Z");
    assert_eq!(instant, Some(1_791_071_890_000));
    assert_eq!(timestamp(b"2026-10-04T01:58:10+02:00"), instant);
    assert_eq!(timestamp(b"2026-10-03T19:58:10-04:00"), instant);
    assert_eq!(timestamp(b"0001791071890000"), instant);
    assert_eq!(timestamp(b"1969-12-31T23:59:59-00:00"), Some(-1000));
    assert_eq!(timestamp(b"1970-01-01T23:59:00+23:59"), Some(0));
    assert_eq!(timestamp(b"1970-01-01T00:00:00-23:59"), Some(DAY - 60_000));
}

#[test]
fn gregorian_leap_days_and_clock_ranges() {
    assert_eq!(
        timestamp(b"2000-03-01T00:00:00Z").unwrap() - timestamp(b"2000-02-29T00:00:00Z").unwrap(),
        DAY
    );
    for bad in [
        b"2100-02-29T00:00:00Z",
        b"2026-02-30T00:00:00Z",
        b"2026-00-01T00:00:00Z",
        b"2026-11-00T00:00:00Z",
        b"2026-11-31T00:00:00Z",
        b"2026-13-01T00:00:00Z",
        b"2026-11-01T24:00:00Z",
        b"2026-11-01T00:60:00Z",
        b"2026-11-01T00:00:60Z",
    ] {
        assert_eq!(timestamp(bad), None);
    }
}

#[test]
fn timestamp_syntax_is_ascii_and_fully_consumed() {
    for bad in [
        "",
        "-5",
        "+1",
        "1.0",
        "12e3",
        "٢٥٠",
        "２５０",
        "2026-10-01 00:00:00Z",
        "2026-10-01t00:00:00Z",
        "2026-10-01T00:00:00z",
        "2026-10-01T00:00:00Z\r",
        "2026-10-01T00:00:00.000Z",
        "2026-10-01T00:00:00+24:00",
        "2026-10-01T00:00:00+05:60",
        "2026-10-01T00:00:00+0200",
        "20a6-10-01T00:00:00Z",
    ] {
        assert_eq!(timestamp(bad.as_bytes()), None, "{bad:?}");
    }
}

#[test]
fn decimal_overflow_and_zero_padding() {
    assert_eq!(timestamp(b"9223372036854775807"), Some(i64::MAX));
    assert_eq!(timestamp(b"9223372036854775808"), None);
    let mut digits = vec![b'0'; 50_000];
    digits.extend_from_slice(b"86400000");
    assert_eq!(timestamp(&digits), Some(DAY));
    assert_eq!(price(b"0000000000000010000000"), Some(10_000_000));
    for bad in [b"".as_slice(), b"0", b"10000001", b"1.0", b"-1", b"42\r"] {
        assert_eq!(price(bad), None);
    }
}

#[test]
fn word_at_a_time_timestamp_checks_each_byte() {
    for valid in [
        b"2028-02-29T23:59:00Z".as_slice(),
        b"2028-03-01T00:59:00+01:00",
        b"1835481540000",
    ] {
        assert!(timestamp(valid).is_some());
        let mut changed = valid.to_vec();
        for (index, &original) in valid.iter().enumerate() {
            if original.is_ascii_digit() {
                for byte in (0..=u8::MAX).filter(|b| !b.is_ascii_digit()) {
                    changed[index] = byte;
                    assert_eq!(timestamp(&changed), None, "{changed:?}");
                }
                changed[index] = original;
            }
        }
    }
}

#[test]
fn compact_event_keeps_high_bits_and_maximum_price() {
    let time = (1_i64 << 31) - 1;
    let event = CompactEvent::new(time, 10_000_000, 511);
    assert_eq!(event.time(), time);
    assert_eq!(event.purchase(), (10_000_000, 511));
}

#[test]
fn exact_key_equality_roundtrip_and_byte_order() {
    let keys = Keys::default();
    let values = [
        b"".as_slice(),
        b"A",
        b"a",
        b"aa",
        b"a\0",
        b"a\0b",
        b"\t",
        b"12345678",
        b"123456789",
        b"1234\x00567",
        b"12345678\0",
        "xé12345".as_bytes(),
        b"1234567890",
        b"1234567891",
        b"12345678900",
        "é".as_bytes(),
        "中".as_bytes(),
    ];
    assert_eq!(keys.get(b""), Key::EMPTY);
    for a in values {
        assert_eq!(keys.bytes(keys.get(a)), a);
        for b in values {
            assert_eq!(keys.get(a) == keys.get(b), a == b);
            assert_eq!(keys.compare(keys.get(a), keys.get(b)), a.cmp(b));
        }
    }
}

#[test]
fn dialect_matches_independent_splitter_exhaustively() {
    fn reference<const N: usize>(row: &[u8]) -> Option<[&[u8]; N]> {
        let mut out = Vec::new();
        for raw in row.split(|&c| c == b',') {
            let value = if raw.len() >= 2 && raw.first() == Some(&b'"') && raw.last() == Some(&b'"')
            {
                &raw[1..raw.len() - 1]
            } else {
                raw
            };
            if value.contains(&b'"') {
                return None;
            }
            out.push(value);
        }
        out.try_into().ok()
    }
    let symbols = [b',', b'"', b'\r', b'x', 0];
    for length in 0..=6 {
        let mut row = vec![0; length];
        for mut code in 0..symbols.len().pow(length as u32) {
            for byte in &mut row {
                *byte = symbols[code % symbols.len()];
                code /= symbols.len();
            }
            assert_eq!(fields::<2>(&row), reference::<2>(&row), "{row:?}");
            assert_eq!(fields::<4>(&row), reference::<4>(&row), "{row:?}");
            assert_eq!(fields::<7>(&row), reference::<7>(&row), "{row:?}");
        }
    }

    // Move delimiters and quote pairs across vector boundaries and through
    // the overlapping final block, including the scalar fallback threshold.
    for pad in 0..=128 {
        let mut row = vec![b'x'; pad];
        row.extend_from_slice(b",\"a\",\"\",zz,99,\"x\",\r");
        assert_eq!(fields::<7>(&row), reference::<7>(&row));
        for position in 0..row.len() {
            let original = row[position];
            for byte in [b',', b'"'] {
                row[position] = byte;
                assert_eq!(fields::<7>(&row), reference::<7>(&row), "{row:?}");
            }
            row[position] = original;
        }
    }
}

#[test]
fn structural_scanner_matches_scalar_rows_at_every_block_offset() {
    fn compare<const N: usize>(data: &[u8]) {
        let expected: Vec<_> = lines(data).filter_map(fields::<N>).collect();
        let mut result = Vec::new();
        crate::csv::records(data, |row: [&[u8]; N]| result.push(row));
        assert_eq!(result, expected);
    }
    let mut data = Vec::new();
    // Include all short records over this dialect alphabet, interleaved
    // with longer rows and good records to catch cross-row contamination.
    let alphabet = b"x,\x22\r\0";
    for mut code in 0..alphabet.len().pow(6) {
        for _ in 0..6 {
            data.push(alphabet[code % alphabet.len()]);
            code /= alphabet.len();
        }
        data.push(b'\n');
    }
    for offset in 0..64 {
        for size in [15, 31, 63, 64, 65, 95, 127, 128, 129, 279] {
            let padding = (64 + offset - data.len() % 64) % 64;
            data.extend(std::iter::repeat_n(b'\n', padding));
            let mut row = vec![b'x'; size];
            row[..5].copy_from_slice(b"a,\"\",");
            row[size - 9..].copy_from_slice(b",\"q\",b,,c");
            data.extend_from_slice(&row);
            data.push(b'\n');
            row[size / 2] = b'"';
            data.extend_from_slice(&row);
            data.push(b'\n');
        }
    }
    data.extend_from_slice(b"last,\"row\",no,lf");
    compare::<2>(&data);
    compare::<4>(&data);
    compare::<7>(&data);
    for len in 0..=192 {
        compare::<4>(&data[..len]);
    }
}

#[test]
fn date_cache_checks_full_dates_across_collisions_and_calendar_changes() {
    let mut dates = crate::parse::DateCache::default();
    for (date, expected) in [
        ("1970-01-01T00:00:00Z", Some(0)),
        ("1970-02-01T00:00:00Z", Some(31 * DAY)),
        ("1971-01-01T00:00:00Z", Some(365 * DAY)),
        ("1970-13-01T00:00:00Z", None),
        ("1970-01-01T00:00:00Z", Some(0)),
        ("1970-01-01T24:00:00Z", None),
        ("1970-01-01T00:01:00+00:01", Some(0)),
    ] {
        assert_eq!(dates.timestamp(date.as_bytes()), expected);
    }
}

#[test]
fn dense_dimension_lookup_keeps_exact_spelling_and_sparse_ids() {
    let names = [
        "p1",
        "p0",
        "p01",
        "p3",
        "p99999999",
        "p100000000",
        "P1",
        "p 1",
        "é",
    ];
    let lookup = crate::catalog::Lookup::new(names.iter().enumerate().map(|(i, &s)| (s, i as u32)));
    for (i, name) in names.iter().enumerate() {
        assert_eq!(lookup.get(name.as_bytes()), Some(i as u32));
    }
    for name in ["p2", "p00", "p999999999999999999999999", "P01", "p"] {
        assert_eq!(lookup.get(name.as_bytes()), None);
    }
}

#[test]
fn compact_event_sort_preserves_every_purchase_and_equal_timestamp() {
    for n in [4095, 4096, 20_003] {
        let mut state = 19_u64;
        let rows = (0..n).map(|i| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let time = if i % 3 == 0 {
                (1 << 31) - 1
            } else {
                (state >> 33) as i64
            };
            CompactEvent::new(time, i as u32 % 997, i as u32 % 512)
        });
        let mut events: Vec<_> = rows.collect();
        let values = |events: &[CompactEvent]| {
            let mut v: Vec<_> = events.iter().map(|e| (e.time(), e.purchase())).collect();
            v.sort_unstable();
            v
        };
        let expected = values(&events);
        CompactEvent::sort(&mut events, &mut Vec::new());
        assert!(events.windows(2).all(|p| p[0].time() <= p[1].time()));
        assert_eq!(values(&events), expected);
    }
}

#[test]
fn chunk_reader_emits_each_physical_row_once_at_exact_boundaries() {
    use crate::input::{Input, Reader};
    use std::fs;
    let temp = tempfile::tempdir().unwrap();
    let block = 1 << 20;
    for (file, pos) in [block - 1, block, block + 1, 2 * block + 9]
        .into_iter()
        .enumerate()
    {
        let mut bytes = vec![b'h'; pos];
        bytes.extend_from_slice(b"\nfirst\r\n");
        bytes.extend(std::iter::repeat_n(b'x', block * 2 + 5));
        bytes.extend_from_slice(b"\n\nlast");
        fs::write(temp.path().join(format!("{file}.csv")), bytes).unwrap();
    }
    let input = Input::open(temp.path()).unwrap();
    let mut reader = Reader::default();
    let mut lengths = Vec::new();
    for chunk in input.chunks() {
        lengths.extend(lines(reader.body(chunk).unwrap()).map(|line| line.len()));
    }
    assert_eq!(lengths, [6, block * 2 + 5, 0, 4].repeat(4));
}
