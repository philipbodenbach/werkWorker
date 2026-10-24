//! Direct chat controls for server-owned reasoning tiers.
use crate::cli_tui::{BRAND, MUTED, TEXT, choice_style, clean, panel};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use werk_worker::{Limits, Result, SessionHandle, providers::ReasoningOptions};

pub fn command(input: &str) -> Option<&str> {
    let (name, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    matches!(name, "/effort" | "/reasoning").then(|| rest.trim())
}

#[derive(Default)]
pub struct Picker {
    pub options: Option<ReasoningOptions>,
    pub selected: usize,
    pub pending: Option<tokio::task::JoinHandle<Result<ReasoningOptions>>>,
    pub applying: bool,
    pub quiet: bool,
}
impl Drop for Picker {
    fn drop(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}
impl Picker {
    pub fn start(
        &mut self,
        session: SessionHandle,
        limits: Limits,
        tier: Option<String>,
        quiet: bool,
    ) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
        self.applying = tier.is_some();
        self.quiet = quiet;
        self.pending = Some(tokio::spawn(async move {
            if let Some(tier) = tier {
                session.set_reasoning_tier(&tier, &limits).await
            } else {
                session.reasoning_options(&limits).await
            }
        }));
    }
    pub fn update(&mut self, options: ReasoningOptions) {
        self.selected = if options.native_effort.is_some() {
            0
        } else {
            Self::tiers(&options)
                .position(|t| t.id == options.requested)
                .map_or(0, |n| n + 1)
        };
        self.options = Some(options);
    }
    fn tiers(
        options: &ReasoningOptions,
    ) -> impl Iterator<Item = &werk_worker::providers::ReasoningTier> {
        options
            .profile
            .iter()
            .filter(|p| p.supported)
            .flat_map(|p| &p.tiers)
            .filter(|t| t.available)
    }
    fn count(&self) -> usize {
        1 + self.options.as_ref().map_or(0, |o| Self::tiers(o).count())
    }
    pub fn move_selection(&mut self, forward: bool) {
        self.selected = if forward {
            (self.selected + 1).min(self.count() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
    }
    pub fn choice(&self) -> Option<String> {
        let options = self.options.as_ref()?;
        if self.selected == 0 {
            return Some("default".into());
        }
        Self::tiers(options)
            .nth(self.selected - 1)
            .map(|t| t.id.clone())
    }
    pub fn label(&self) -> String {
        self.options.as_ref().map_or_else(
            || "Reasoning: /effort".into(),
            |o| format!("Reasoning: {}", o.label()),
        )
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        // A small slider above the composer; keep the conversation visible.
        let height = area.height.min(9);
        let area = Rect::new(
            area.x,
            area.bottom().saturating_sub(height),
            area.width.min(82),
            height,
        );
        frame.render_widget(Clear, area);
        let block = panel(" Reasoning effort · WerkProtocol ", BRAND[3]);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(options) = &self.options else {
            frame.render_widget(
                Paragraph::new(if self.pending.is_some() {
                    "Loading model tiers from WerkProtocol…"
                } else {
                    "Reasoning profile unavailable. Esc returns to chat."
                }),
                inner,
            );
            return;
        };
        let mut labels = vec!["Default".to_owned()];
        labels.extend(Self::tiers(options).map(|t| clean(&t.label)));
        let width = inner.width as usize;
        let mut start = 0;
        // Scroll long/custom tier lists horizontally, always keeping the chosen stop visible.
        while start < self.selected
            && labels[start..=self.selected]
                .iter()
                .map(|s| Line::from(s.as_str()).width() + 6)
                .sum::<usize>()
                + 2
                > width
        {
            start += 1;
        }
        let mut slider = vec![Span::raw(if start > 0 { "‹ " } else { "  " })];
        let mut used = 2;
        for (i, label) in labels.iter().enumerate().skip(start) {
            let text = format!("{} {label} ", if i == self.selected { "●" } else { "○" });
            let len = Line::from(text.as_str()).width();
            if i > self.selected && used + len + 2 > width {
                slider.push(Span::raw(" ›"));
                break;
            }
            slider.push(Span::styled(
                text,
                if i == self.selected {
                    choice_style(true)
                } else {
                    Style::default().fg(MUTED)
                },
            ));
            if i + 1 < labels.len() {
                slider.push(Span::raw("── "));
            }
            used += len + 3;
        }
        let detail = if self.selected == 0 {
            let mut default = options.clone();
            default.requested = "default".into();
            default.native_effort = None;
            default.label()
        } else {
            Self::tiers(options)
                .nth(self.selected - 1)
                .map(|t| {
                    format!(
                        "{}{}",
                        t.id,
                        t.reasoning_tokens
                            .map(|n| format!(" · {n} reasoning tokens"))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_default()
        };
        let limits = options
            .profile
            .as_ref()
            .map(|p| {
                format!(
                    "Valid for {} output tokens · {} answer reserve",
                    p.limits.output_tokens, p.limits.answer_reserve_tokens
                )
            })
            .unwrap_or_else(|| "Server has not reported additional tiers.".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(clean(&format!("Current: {}", options.label()))),
                Line::from(""),
                Line::from(slider),
                Line::from(clean(&detail)),
                Line::styled(limits, Style::default().fg(MUTED)),
                Line::from(if self.pending.is_some() {
                    "Loading / applying…"
                } else {
                    "←/→ select · Enter apply · Esc cancel"
                }),
                Line::styled(
                    "Saved for this session · takes effect on the next planned request",
                    Style::default().fg(MUTED),
                ),
            ])
            .style(Style::default().fg(TEXT)),
            inner,
        );
    }
}
