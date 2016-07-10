//! Utility functions for working with keys.

use std::time::SystemTime;

use byteorder::{BigEndian, ByteOrder, WriteBytesExt, NativeEndian};

use DataType;
use Error;
use Result;
use Row;
use Schema;
use util::{time_to_us, us_to_time};

/// Murmur2 hash implementation returning 64-bit hashes.
pub fn murmur2_64(mut data: &[u8], seed: u64) -> u64 {
    let m: u64 = 0xc6a4a7935bd1e995;
    let r: u8 = 47;

    let mut h : u64 = seed ^ ((data.len() as u64).wrapping_mul(m));

    while data.len() >= 8 {
        let mut k = NativeEndian::read_u64(data);

        k = k.wrapping_mul(m);
        k ^= k >> r;
        k = k.wrapping_mul(m);
        h ^= k;
        h = h.wrapping_mul(m);

        data = &data[8..];
    };

    let len = data.len();
    if len > 6 { h ^= (data[6] as u64) << 48; }
    if len > 5 { h ^= (data[5] as u64) << 40; }
    if len > 4 { h ^= (data[4] as u64) << 32; }
    if len > 3 { h ^= (data[3] as u64) << 24; }
    if len > 2 { h ^= (data[2] as u64) << 16; }
    if len > 1 { h ^= (data[1] as u64) << 8; }
    if len > 0 { h ^= data[0] as u64;
                 h = h.wrapping_mul(m) }

    h ^= h >> r;
    h = h.wrapping_mul(m);
    h ^= h >> r;
    h
}

pub fn encode_primary_key(row: &Row) -> Vec<u8> {
    let mut buf = Vec::new();
    let num_primary_key_columns = row.schema().num_primary_key_columns();

    for idx in 0..num_primary_key_columns {
        encode_column(row, idx, idx + 1 == num_primary_key_columns, &mut buf);
    }
    buf
}

fn encode_column(row: &Row, idx: usize, is_last: bool, buf: &mut Vec<u8>) {
    match row.schema().columns()[idx].data_type() {
        DataType::Bool => buf.push(if row.get::<bool>(idx).unwrap() { 1 } else { 0 }),
        DataType::Int8 => buf.push(row.get::<i8>(idx).unwrap() as u8),
        DataType::Int16 => buf.write_i16::<BigEndian>(row.get::<i16>(idx).unwrap()).unwrap(),
        DataType::Int32 => buf.write_i32::<BigEndian>(row.get::<i32>(idx).unwrap()).unwrap(),
        DataType::Int64 => buf.write_i32::<BigEndian>(row.get::<i32>(idx).unwrap()).unwrap(),
        DataType::Timestamp => buf.write_i64::<BigEndian>(time_to_us(&row.get::<SystemTime>(idx).unwrap())).unwrap(),
        DataType::Float => buf.write_f32::<BigEndian>(row.get::<f32>(idx).unwrap()).unwrap(),
        DataType::Double => buf.write_f64::<BigEndian>(row.get::<f64>(idx).unwrap()).unwrap(),
        DataType::Binary => encode_binary(row.get(idx).unwrap(), is_last, buf),
        DataType::String => encode_binary(row.get::<&str>(idx).unwrap().as_bytes(), is_last, buf),
    }
}

fn encode_binary(value: &[u8], is_last: bool, buf: &mut Vec<u8>) {
    if is_last {
        buf.extend_from_slice(value);
    } else {
        for split in value.split(|&x| x == 0) {
            buf.extend_from_slice(split);
            buf.extend_from_slice(&[0, 1]);
        }
        let len = buf.len();
        buf[len-1] = 0;
    }
}

pub fn decode_primary_key(schema: &Schema, mut key: &[u8]) -> Result<Row> {
    let mut row = schema.new_row();

    let num_primary_key_columns = row.schema().num_primary_key_columns();
    for idx in 0..num_primary_key_columns {
        key = try!(decode_column(&mut row, idx, idx + 1 == num_primary_key_columns, key));
    }

    if !key.is_empty() {
        return Err(Error::Serialization(
                "bytes remaining after decoding all key key columns".to_owned()));
    }
    Ok(row)
}

fn decode_column<'a>(row: &mut Row, idx: usize, is_last: bool, key: &'a [u8]) -> Result<&'a [u8]> {
    match row.schema().columns()[idx].data_type() {
        DataType::Bool => {
            try!(row.set(idx, if key[0] == 0 { false } else { true }));
            Ok(&key[1..])
        },
        DataType::Int8 => {
            try!(row.set(idx, key[0] as i8));
            Ok(&key[1..])
        },
        DataType::Int16 => {
            try!(row.set(idx, BigEndian::read_i16(key)));
            Ok(&key[2..])
        },
        DataType::Int32 => {
            try!(row.set(idx, BigEndian::read_i32(key)));
            Ok(&key[4..])
        },
        DataType::Int64 =>  {
            try!(row.set(idx, BigEndian::read_i64(key)));
            Ok(&key[8..])
        },
        DataType::Timestamp => {
            try!(row.set(idx, us_to_time(BigEndian::read_i64(key))));
            Ok(&key[8..])
        },
        DataType::Float => {
            try!(row.set(idx, BigEndian::read_f32(key)));
            Ok(&key[4..])
        },
        DataType::Double => {
            try!(row.set(idx, BigEndian::read_f64(key)));
            Ok(&key[8..])
        },
        DataType::Binary => {
            let (remaining, value) = try!(decode_binary(key, is_last));
            try!(row.set(idx, value));
            Ok(remaining)
        },
        DataType::String => {
            let (remaining, value) = try!(decode_binary(key, is_last));
            try!(row.set(idx, try!(String::from_utf8(value))));
            Ok(remaining)
        },
    }
}

fn decode_binary(mut key: &[u8], is_last: bool) -> Result<(&[u8], Vec<u8>)> {
    if is_last {
        Ok((&[], key.to_owned()))
    } else {
        let mut ret = Vec::new();

        loop {
            match key.iter().position(|&x| x == 0) {
                Some(idx) => match key.get(idx + 1) {
                    Some(&b) => match b {
                        0 => {
                            ret.extend_from_slice(&key[..idx]);
                            key = &key[idx+2..];
                            break;
                        },
                        1 => {
                            ret.extend_from_slice(&key[..idx+1]);
                            key = &key[idx+2..];
                        }
                        _ => return Err(Error::Serialization(format!(
                                    "unexpected binary sequence: {:?}", &[0, b])))

                    },
                    None => return Err(Error::Serialization(
                            "expected binary column separator sequence".to_owned())),
                },
                None => return Err(Error::Serialization(
                        "expected binary column separator sequence".to_owned())),
            }
        }

        Ok((key, ret))
    }
}


#[cfg(test)]
mod test {

    use SchemaBuilder;
    use DataType;

    use super::*;

    #[test]
    fn test_murmur2_64() {
        assert_eq!(7115271465109541368, murmur2_64(b"ab", 0));
        assert_eq!(2601573339036254301, murmur2_64(b"abcdefg", 0));
        assert_eq!(3575930248840144026, murmur2_64(b"quick brown fox", 42));
    }

    #[test]
    fn primary_key_encode_decode() {
        let mut builder = SchemaBuilder::new();
        builder.add_column("a", DataType::String).set_not_null();
        builder.add_column("b", DataType::Int32).set_not_null();
        builder.add_column("c", DataType::String).set_not_null();
        builder.set_primary_key(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        let schema = builder.build().unwrap();

        {
            let mut row = schema.new_row();
            assert_eq!(row.schema(), &schema);
            row.set(0, "fuzz\0\0\0\0buster").unwrap();
            row.set(1, 99).unwrap();
            row.set(2, "calibri\0\0\0").unwrap();
            let key = encode_primary_key(&row);

            let decoded_row = decode_primary_key(&schema, &key).unwrap();
            assert_eq!(row, decoded_row);
        }
    }
}
