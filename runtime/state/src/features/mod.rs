//! Bodies of the `StateBackend` methods the engine features add, shared by
//! the SQLite backends. Each takes the scope's tenant as `Option<&str>`:
//! `None` on the unscoped backend. SQL text is built only from literals
//! chosen by those options; every value is bound.

pub(crate) mod run_keys;
