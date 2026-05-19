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
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>>
    {
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
