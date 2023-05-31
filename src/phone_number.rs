//! A library to implement phone numbers for our database and JSON serialization and deserialization.

use std::str::FromStr;

use diesel::{backend::RawValue, serialize::ToSql, sql_types::Text, AsExpression, FromSqlRow};

/// A phone number.
#[derive(Debug, Default, Clone, PartialEq, Hash, Eq, FromSqlRow, AsExpression)]
#[diesel(sql_type = Text)]
pub struct PhoneNumber(pub Option<phonenumber::PhoneNumber>);

impl From<phonenumber::PhoneNumber> for PhoneNumber {
    fn from(id: phonenumber::PhoneNumber) -> PhoneNumber {
        PhoneNumber(Some(id))
    }
}

impl AsRef<Option<phonenumber::PhoneNumber>> for PhoneNumber {
    fn as_ref(&self) -> &Option<phonenumber::PhoneNumber> {
        &self.0
    }
}

impl std::ops::Deref for PhoneNumber {
    type Target = Option<phonenumber::PhoneNumber>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DB> ToSql<Text, DB> for PhoneNumber
where
    DB: diesel::backend::Backend<
        BindCollector = diesel::query_builder::bind_collector::RawBytesBindCollector<DB>,
    >,
    String: diesel::serialize::ToSql<Text, DB>,
{
    fn to_sql<'a>(
        &'a self,
        out: &mut diesel::serialize::Output<'a, '_, DB>,
    ) -> diesel::serialize::Result {
        String::to_sql(&self.to_string(), &mut out.reborrow())
    }
}

impl<DB> diesel::deserialize::FromSql<Text, DB> for PhoneNumber
where
    DB: diesel::backend::Backend,
    String: diesel::deserialize::FromSql<Text, DB>,
{
    fn from_sql(bytes: RawValue<DB>) -> diesel::deserialize::Result<Self> {
        let s = String::from_sql(bytes)?;
        Ok(PhoneNumber::from_str(&s)?)
    }
}

impl std::fmt::Display for PhoneNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = if let Some(phone) = &self.0 {
            phone
                .format()
                .mode(phonenumber::Mode::International)
                .to_string()
        } else {
            String::new()
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for PhoneNumber {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().is_empty() {
            return Ok(PhoneNumber(None));
        }

        let s = if !s.trim().starts_with('+') {
            format!("+1{s}").replace(['-', '(', ')', ' '], "")
        } else {
            s.replace(['-', '(', ')', ' '], "")
        };

        Ok(PhoneNumber(Some(phonenumber::parse(None, &s).map_err(
            |e| anyhow::anyhow!("invalid phone number `{}`: {}", s, e),
        )?)))
    }
}
