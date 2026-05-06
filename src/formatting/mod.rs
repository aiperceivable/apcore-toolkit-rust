// Formatting utilities.

mod markdown;
mod surface;

pub use markdown::{to_markdown, MarkdownError, MarkdownOptions};
pub use surface::{
    format_module, format_modules, format_schema, FormatError, FormatOutput, GroupBy, ModuleStyle,
    SchemaStyle,
};
