//! Building store queries without a caller's value in the SQL text.

use crate::scope::Scope;
use sqlx::{Database, Encode, QueryBuilder, Type};

/// Append the conditions that select `scope`: `org_id = <bound>`, then one
/// `AND <column> = <bound>` for each id the scope sets.
///
/// Every value is a bound parameter. The stores used to write the ids into
/// the text with their single quotes doubled, which is not an escape on a
/// Postgres server with `standard_conforming_strings` off: an id ending in
/// `\'` closed its own literal, and a delete for one organisation matched
/// every row. Only the column names, which are literals here, reach the text.
pub(crate) fn push_scope<DB>(query: &mut QueryBuilder<DB>, scope: &Scope)
where
    DB: Database,
    String: for<'t> Encode<'t, DB> + Type<DB>,
{
    query.push("org_id = ").push_bind(scope.org_id.clone());
    for (column, value) in [
        ("user_id", &scope.user_id),
        ("agent_id", &scope.agent_id),
        ("session_id", &scope.session_id),
    ] {
        if let Some(value) = value {
            query
                .push(" AND ")
                .push(column)
                .push(" = ")
                .push_bind(value.clone());
        }
    }
}
