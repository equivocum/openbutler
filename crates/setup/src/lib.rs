/* Setup as a library: the generic `openbutler` CLI shares `.env`
handling + config rendering with `openbutler-setup` instead of
copying them. Runtime crates must never depend on this. */

pub mod envfile;
pub mod render;
