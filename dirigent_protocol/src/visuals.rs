//! Visuals a reply shows. The model writes HTML and CSS in a fenced code block with the info
//! string `FENCE`; a client draws that as a picture `WIDTH` CSS px wide and as tall as its
//! content, in the block's place. No scripts run, and nothing the document links to is loaded.

pub const FENCE: &str = "visual";

pub const WIDTH: u32 = 720;
