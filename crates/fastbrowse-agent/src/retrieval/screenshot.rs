//! Reading what a drawn page shows: the text in its pixels, which its text layer does not carry, and how it looks.
//!
//! A chart's labels, a sign's price and a picture's caption exist only in the image. The model that can see it reads
//! them as a quote, and the fact's evidence names the screenshot's digest so a citation says where the words came from.

use fastbrowse_core::llm::{Generation, LLMClient, Message};
use fastbrowse_core::models::{Bytes, LLMPurpose, UNTRUSTED};
use fastbrowse_core::py::lax;
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::Ledger;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::support::generate;

const IMAGE_READ: &str = "What the screenshot shows that answers the task: its text quoted as the image shows it, or, where the task asks how the page looks, what the image shows of that, described as seen. Null when the image shows nothing that answers the task.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageReading {
    #[serde(deserialize_with = "lax::nullable")]
    pub text: Option<String>,
}

impl Schema for ImageReading {
    fn ty() -> Ty {
        let field = Field::required("text", Ty::optional(Ty::Str)).doc(IMAGE_READ);
        ModelTy { module: "fastbrowse.retrieval", ..ModelTy::frozen("ImageReading", vec![field]) }.ty()
    }
}

const READ_INSTRUCTIONS: &str = "# Screenshot\nThe task's answer may be carried by the page's pixels rather than its text: a chart's labels, a sign's price, a picture's caption, or how the page looks. Read the attached viewport screenshot and quote the text it shows that answers the task, in the form the image shows it. Where the task asks how the page looks, such as its colours, styling, artwork or layout, describe what the image shows of that, plainly and as seen. Give null when the image shows nothing that answers the task. Never infer, estimate or supply a value or a look the image does not show.\n\n# Trust\n";

/// The text a screenshot shows that answers the task, read by the model that can see it.
///
/// The quote is the model's reading of an image, so it is recorded against the screenshot's digest and marked as not
/// literal page text. A run whose answer is only visible in pixels otherwise reports that nothing answers the task,
/// while a person looking at the same screen reads the value off it.
pub async fn read_image(
    llm: &dyn LLMClient,
    task: &str,
    png: Vec<u8>,
    ledger: Option<&Ledger>,
) -> Result<Generation<ImageReading>> {
    let mut user =
        Message::user(format!("# Task\n{task}\n\n# Screenshot\nThe page's viewport screenshot is attached."));
    user.images.push(Bytes(png));
    let messages = [Message::system(format!("{READ_INSTRUCTIONS}{UNTRUSTED}")), user];
    Ok(generate::<ImageReading>(llm, LLMPurpose::ImageRead, &messages, ledger).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_is_told_never_to_supply_a_value_the_image_does_not_show() {
        // The answer's citation names the screenshot as its source, so the reading must stay what the pixels show.
        let instructions = format!("{READ_INSTRUCTIONS}{UNTRUSTED}");
        assert!(instructions.contains("Give null when the image shows nothing that answers the task"));
        assert!(instructions.contains("Never infer, estimate or supply a value or a look the image does not show"));
        // A page styled by CSS states its colours nowhere in its text, so its look is read off the image.
        assert!(instructions.contains("Where the task asks how the page looks"));
        assert!(instructions.contains(UNTRUSTED));
    }
}
