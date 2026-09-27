// Adapted for Timon. Not derived from Prodex source.
//! The tool a lead uses to delegate work.
//!
//! Everything before this built the pieces — supervision, typed results, usage
//! accounting, admission, citation checking — but nothing let a lead *use* them.
//! A lead is a model session, and a model can only do what its tools allow, so
//! until there is a tool there is no delegation and no orchestration.
//!
//! It has to be a tool rather than a shell command the lead runs itself. The
//! lead executes under a sandbox that denies the network, which qualification
//! established directly, so a worker started from inside the lead's own shell
//! could never reach a provider. An MCP server is launched by the harness rather
//! than by the model, so the workers it starts are outside that sandbox.

pub mod protocol;
pub mod server;
pub mod tools;
