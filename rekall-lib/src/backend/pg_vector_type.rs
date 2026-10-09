// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

use bytes::{BufMut, BytesMut};
use postgres_types::{to_sql_checked, IsNull, ToSql, Type};

/// Wrapper for encoding a float slice as pgvector's binary format.
/// Binary format: u16 dim + u16 unused(0) + f32[dim] (big-endian)
#[derive(Debug)]
pub struct PgVector<'a>(pub &'a [f32]);

impl ToSql for PgVector<'_> {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        let dim = self.0.len() as u16;
        out.put_u16(dim);
        out.put_u16(0); // unused
        for &v in self.0 {
            out.put_f32(v);
        }
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Look up pgvector's vector type OID and create a Type for it.
pub async fn lookup_vector_type(
    client: &tokio_postgres::Client,
) -> Result<Type, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT t.oid, t.typname, n.nspname \
             FROM pg_type t JOIN pg_namespace n ON t.typnamespace = n.oid \
             WHERE t.typname = 'vector' LIMIT 1",
            &[],
        )
        .await?;

    let oid: u32 = row.get(0);
    Ok(Type::new(
        "vector".to_string(),
        oid,
        postgres_types::Kind::Simple,
        "public".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn encode_empty_vector() {
        let v = PgVector(&[]);
        let mut buf = BytesMut::new();
        let ty = Type::BYTEA; // type doesn't matter for encoding
        v.to_sql(&ty, &mut buf).unwrap();
        assert_eq!(buf.len(), 4); // 2 bytes dim + 2 bytes unused
        assert_eq!(&buf[..2], &[0, 0]); // dim = 0
        assert_eq!(&buf[2..4], &[0, 0]); // unused = 0
    }

    #[test]
    fn encode_single_element() {
        let v = PgVector(&[1.0]);
        let mut buf = BytesMut::new();
        v.to_sql(&Type::BYTEA, &mut buf).unwrap();
        assert_eq!(buf.len(), 4 + 4); // header + 1 float
        assert_eq!(&buf[..2], &[0, 1]); // dim = 1
        assert_eq!(&buf[2..4], &[0, 0]); // unused = 0
        let float_bytes = &buf[4..8];
        assert_eq!(float_bytes, &1.0_f32.to_be_bytes());
    }

    #[test]
    fn encode_multiple_elements() {
        let data = [1.0_f32, 2.0, 3.0];
        let v = PgVector(&data);
        let mut buf = BytesMut::new();
        v.to_sql(&Type::BYTEA, &mut buf).unwrap();
        assert_eq!(buf.len(), 4 + 12); // header + 3 floats
        assert_eq!(&buf[..2], &[0, 3]); // dim = 3

        for (i, &expected) in data.iter().enumerate() {
            let offset = 4 + i * 4;
            let bytes: [u8; 4] = buf[offset..offset + 4].try_into().unwrap();
            assert_eq!(f32::from_be_bytes(bytes), expected);
        }
    }

    #[test]
    fn encode_preserves_float_precision() {
        let data = [0.123_456_79_f32, -0.987_654_3, f32::MIN, f32::MAX];
        let v = PgVector(&data);
        let mut buf = BytesMut::new();
        v.to_sql(&Type::BYTEA, &mut buf).unwrap();

        for (i, &expected) in data.iter().enumerate() {
            let offset = 4 + i * 4;
            let bytes: [u8; 4] = buf[offset..offset + 4].try_into().unwrap();
            assert_eq!(f32::from_be_bytes(bytes), expected);
        }
    }
}
