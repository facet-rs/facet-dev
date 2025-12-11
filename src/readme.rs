pub struct GenerateReadmeOpts {
    pub crate_name: String,
    pub input: String,
    pub header: Option<String>,
    pub footer: Option<String>,
}

pub fn generate(opts: GenerateReadmeOpts) -> String {
    // Generate header by replacing "{CRATE}" in the provided or default header template
    let header_template = opts.header.as_deref().unwrap_or(include_str!("header.md"));
    let header = header_template.replace("{CRATE}", &opts.crate_name);

    // The main template content, passed in via `opts.input`
    let template_content = opts.input;

    // Use provided footer or default footer template
    let footer = opts
        .footer
        .as_deref()
        .unwrap_or(include_str!("footer.md"))
        .to_string();

    // Combine header, template, and footer with newlines
    format!("{header}\n{template_content}\n{footer}")
}
