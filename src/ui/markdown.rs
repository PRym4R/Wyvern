use eframe::egui::{Color32, RichText, Ui};

pub(crate) fn render_message_text(ui: &mut Ui, text: &str, size: f32, color: Color32) {
    let mut in_code = false;
    let mut code_buf = String::new();

    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            if in_code && !code_buf.is_empty() {
                ui.label(
                    RichText::new(code_buf.trim_end_matches('\n'))
                        .monospace()
                        .size(size)
                        .color(color)
                        .background_color(Color32::from_gray(28)),
                );
                code_buf.clear();
            }
            in_code = !in_code;
            continue;
        }

        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            ui.add_space(3.0);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            ui.indent("quote", |ui| {
                ui.label(RichText::new(rest).italics().size(size).color(color));
            });
            continue;
        }

        ui.horizontal_wrapped(|ui| {
            render_inline(ui, line, size, color);
        });
    }

    if in_code && !code_buf.is_empty() {
        ui.label(
            RichText::new(code_buf.trim_end_matches('\n'))
                .monospace()
                .size(size)
                .color(color)
                .background_color(Color32::from_gray(28)),
        );
    }
}

fn render_inline(ui: &mut Ui, text: &str, size: f32, color: Color32) {
    let mut pos = 0usize;
    let mut plain = String::new();

    let flush_plain = |ui: &mut Ui, plain: &mut String, size: f32, color: Color32| {
        if !plain.is_empty() {
            ui.label(RichText::new(std::mem::take(plain)).size(size).color(color));
        }
    };

    while pos < text.len() {
        let rest = &text[pos..];
        let mut matched = false;

        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[2..2 + end])
                        .size(size)
                        .color(color)
                        .strong(),
                );
                pos += 4 + end;
                matched = true;
            }
        } else if rest.starts_with("__") {
            if let Some(end) = rest[2..].find("__") {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[2..2 + end])
                        .size(size)
                        .color(color)
                        .underline(),
                );
                pos += 4 + end;
                matched = true;
            }
        } else if rest.starts_with("~~") {
            if let Some(end) = rest[2..].find("~~") {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[2..2 + end])
                        .size(size)
                        .color(color)
                        .strikethrough(),
                );
                pos += 4 + end;
                matched = true;
            }
        } else if rest.starts_with("||") {
            if let Some(end) = rest[2..].find("||") {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[2..2 + end])
                        .size(size)
                        .color(Color32::from_gray(110)),
                );
                pos += 4 + end;
                matched = true;
            }
        } else if rest.starts_with('`') {
            if let Some(end) = rest[1..].find('`') {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[1..1 + end])
                        .monospace()
                        .size(size)
                        .color(color)
                        .background_color(Color32::from_gray(28)),
                );
                pos += 2 + end;
                matched = true;
            }
        } else if rest.starts_with('*') {
            if let Some(end) = rest[1..].find('*') {
                flush_plain(ui, &mut plain, size, color);
                ui.label(
                    RichText::new(&rest[1..1 + end])
                        .size(size)
                        .color(color)
                        .italics(),
                );
                pos += 2 + end;
                matched = true;
            }
        }

        if matched {
            continue;
        }

        // Linked text: [label](url)
        if rest.starts_with('[') {
            if let Some(bracket) = rest.find("](") {
                let label = &rest[1..bracket];
                let tail = &rest[bracket + 2..];
                if let Some(paren) = tail.find(')') {
                    let url = &tail[..paren];
                    flush_plain(ui, &mut plain, size, color);
                    ui.hyperlink_to(label, url);
                    pos += bracket + 2 + paren + 1;
                    continue;
                }
            }
        }

        // Bare URLs.
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| c.is_whitespace())
                .unwrap_or(rest.len());
            flush_plain(ui, &mut plain, size, color);
            ui.hyperlink_to(&rest[..end], &rest[..end]);
            pos += end;
            continue;
        }

        let c = rest.chars().next().unwrap();
        plain.push(c);
        pos += c.len_utf8();
    }

    flush_plain(ui, &mut plain, size, color);
}