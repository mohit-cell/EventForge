use crate::input::{after_header, lines};
use crate::keys::{Key, Keys};
use crate::parse::{DAY, fields, price, timestamp};

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
