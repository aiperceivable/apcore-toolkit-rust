// Display overlay — sparse binding.yaml display resolution (§5.13).
//
// Resolves surface-facing presentation fields (alias, description, guidance)
// for each ScannedModule by merging:
//   surface-specific override > display default > binding-level > scanner value

mod resolver;

pub use resolver::{DisplayResolver, DisplayResolverError};
