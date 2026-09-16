// Setup as a library: the `openbutler` CLI shares `.env` handling and
// rendering instead of copying them. Runtime crates must never depend here.

pub mod envfile;
pub mod render;
