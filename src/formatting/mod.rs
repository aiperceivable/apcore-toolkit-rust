// Formatting utilities.

mod markdown;
mod surface;
mod tabular;

pub use markdown::{to_markdown, MarkdownError, MarkdownOptions};
pub use surface::{
    format_module, format_modules, format_schema, FormatError, FormatOutput, GroupBy, ModuleStyle,
    SchemaStyle,
};
pub use tabular::{format_csv, format_jsonl};
