//! Right pane: the selected item's detail view (metadata, reviewers, body).

use super::*;

pub(super) fn draw_item_detail(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::ItemDetail;
    let block = pane_block("Detail", focused);

    let text = match app.selected_item() {
        None => vec![Line::from(Span::raw("No item selected"))],
        Some(item) => {
            let repo = item.repo_display();
            let author = item
                .author
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_else(|| "—".to_string());
            let state = item.state.clone();
            let title = item.title.clone();
            let updated_at = glauca_core::time::format_local_datetime(&item.updated_at);
            let created_at = item
                .created_at_item
                .as_deref()
                .map(glauca_core::time::format_local_datetime)
                .unwrap_or_else(|| "—".to_string());
            let url = item.url.clone();
            let number = item.number;
            let comment_count = item.comment_count;
            let item_icon = app.icons.item_icon(&item.kind, &state);
            let is_pr = item.kind == "pull_request";

            let labels = if item.labels.is_empty() {
                "—".to_string()
            } else {
                item.labels.join(", ")
            };
            let assignees = if item.assignees.is_empty() {
                "—".to_string()
            } else {
                item.assignees
                    .iter()
                    .map(|u| u.login.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let milestone = item.milestone.clone().unwrap_or_else(|| "—".to_string());

            // Submitted reviews + pending requests.
            let reviewed_logins: std::collections::HashSet<&str> =
                item.reviews.iter().map(|(u, _)| u.login.as_str()).collect();
            // Collected as (badge, style, login) groups so the wrap below can pack them
            // onto lines as indivisible units.
            let mut reviewer_groups: Vec<(&'static str, Style, String)> = Vec::new();
            for (user, state) in &item.reviews {
                let (badge, style) = app.icons.review_state_badge(state);
                reviewer_groups.push((badge, style, user.login.clone()));
            }
            for user in &item.requested_reviewers {
                if !reviewed_logins.contains(user.login.as_str()) {
                    reviewer_groups.push((
                        app.icons.pending_reviewer_icon(user.kind),
                        Style::default().fg(Color::Yellow),
                        user.login.clone(),
                    ));
                }
            }

            // Wrapped by hand so a break never lands inside an "icon  login" group:
            // ratatui's word wrap splits on the icon↔login space, and NBSP does not help
            // because it counts as whitespace there too.
            let label = "Reviewers:";
            let label_style = Style::default().fg(Color::Gray);
            let gap = "  "; // gap between a reviewer's icon and login
            let indent = label.width() + 1; // label + the single space after it
            let inner_w = block.inner(area).width as usize; // content width, sans border/padding
            let sep_w = 3usize; // spaces between reviewers
            let reviewer_lines: Vec<Line> = {
                let mut lines: Vec<Line> = Vec::new();
                if reviewer_groups.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(label, label_style),
                        Span::raw(" —"),
                    ]));
                } else {
                    let mut cur: Vec<Span> = vec![Span::styled(label, label_style), Span::raw(" ")];
                    let mut used_w = indent;
                    let mut first_on_line = true;
                    for (badge, style, login) in reviewer_groups {
                        let group_w = badge.width() + gap.width() + login.width();

                        // Move to a fresh, indented line when this group (with its
                        // separator) would overflow — but never break before the
                        // first group on a line, even if it overflows alone.
                        if !first_on_line && used_w + sep_w + group_w > inner_w {
                            lines.push(Line::from(std::mem::take(&mut cur)));
                            cur.push(Span::raw(" ".repeat(indent)));
                            used_w = indent;
                            first_on_line = true;
                        }
                        if !first_on_line {
                            cur.push(Span::raw(" ".repeat(sep_w)));
                            used_w += sep_w;
                        }
                        cur.push(Span::styled(badge, style));
                        cur.push(Span::raw(format!("{gap}{login}")));
                        used_w += group_w;
                        first_on_line = false;
                    }
                    lines.push(Line::from(cur));
                }
                lines
            };

            let mut lines = vec![
                // Title header: the same kind+state icon as the list row.
                Line::from(vec![
                    Span::styled(item_icon, state_style(&state)),
                    Span::styled(format!("  #{number} "), Style::default().fg(Color::Cyan)),
                    Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::default(),
                // Metadata block
                Line::from(vec![
                    Span::styled("Repo:     ", Style::default().fg(Color::Gray)),
                    Span::raw(repo),
                ]),
                Line::from(vec![
                    Span::styled("Author:   ", Style::default().fg(Color::Gray)),
                    Span::raw(author),
                ]),
                Line::from({
                    // The header icon's colour already carries the state, so no badge here.
                    let mut spans = vec![
                        Span::styled("State:    ", Style::default().fg(Color::Gray)),
                        Span::styled(state.clone(), state_style(&state)),
                    ];
                    if is_pr && item.is_draft {
                        spans.push(Span::styled(
                            "  [Draft]",
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                    spans
                }),
                Line::from(vec![
                    Span::styled("Created:  ", Style::default().fg(Color::Gray)),
                    Span::raw(created_at),
                ]),
                Line::from(vec![
                    Span::styled("Updated:  ", Style::default().fg(Color::Gray)),
                    Span::raw(updated_at),
                ]),
                Line::default(),
                Line::from(vec![
                    Span::styled("Labels:   ", Style::default().fg(Color::Gray)),
                    Span::raw(labels),
                ]),
                Line::from(vec![
                    Span::styled("Milestone:", Style::default().fg(Color::Gray)),
                    Span::raw(format!(" {milestone}")),
                ]),
                Line::from(vec![
                    Span::styled("Assignees:", Style::default().fg(Color::Gray)),
                    Span::raw(format!(" {assignees}")),
                ]),
            ];
            lines.extend(reviewer_lines);
            lines.push(Line::from(vec![
                Span::styled("Comments: ", Style::default().fg(Color::Gray)),
                Span::raw(format!("{comment_count}")),
            ]));

            // PR-only fields
            if is_pr {
                if let (Some(base), Some(head)) = (&item.base_ref, &item.head_ref) {
                    lines.push(Line::from(vec![
                        Span::styled("Branch:   ", Style::default().fg(Color::Gray)),
                        Span::raw(format!("{head} → {base}")),
                    ]));
                }
                if let Some(rd) = &item.review_decision {
                    let (icon, style) = app.icons.review_decision_badge(rd);
                    let badge = match rd.as_str() {
                        "APPROVED" => format!("{icon}  APPROVED"),
                        "CHANGES_REQUESTED" => format!("{icon}  CHANGES REQUESTED"),
                        "REVIEW_REQUIRED" => format!("{icon}  REVIEW REQUIRED"),
                        other => other.to_string(),
                    };
                    lines.push(Line::from(vec![
                        Span::styled("Review:   ", Style::default().fg(Color::Gray)),
                        Span::styled(badge, style),
                    ]));
                }
            }

            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("URL:      ", Style::default().fg(Color::Gray)),
                Span::styled(
                    url,
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));

            // Description body
            if let Some(body) = &item.body
                && !body.is_empty()
            {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "─── Description ────────────────────────────────",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.extend(render_markdown(body));
            }

            lines
        }
    };

    let para = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));

    f.render_widget(para, area);
}
