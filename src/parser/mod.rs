//! Bidirectional parser between Teams HTML / Adaptive Cards / OneNote XHTML, Plain Text / Markdown, and Rich Terminal AST.

pub mod card_in;
pub mod html_in;
pub mod md_out;
pub mod onenote;

pub use onenote::{
    build_append_markdown_payload, build_replace_block_patch, build_replace_page_body_commands,
    build_set_tag_patch, build_toggle_todo_patch, compile_new_page_payload,
    markdown_to_onenote_html_fragment, onenote_blocks_to_markdown, onenote_blocks_to_plain,
    parse_onenote_page_xhtml,
};

use crate::models::{
    AttachmentInfo, InlineImageUpload, MentionInfo, OutgoingMessage, QuotedReply, RichBlock,
    UploadedReferenceAttachment,
};

/// Strips HTML tags and formats Adaptive Cards into clean plain text for previews & FTS5 indexing.
pub fn html_and_cards_to_plain(html: &str, attachments: &[AttachmentInfo]) -> String {
    let blocks = html_in::parse_message_blocks(html, attachments, &[]);
    html_in::blocks_to_plain(&blocks)
}

/// Converts a Teams HTML message + attachments into clean GitHub-flavored Markdown (used by `export --format md`).
pub fn html_and_cards_to_markdown(html: &str, attachments: &[AttachmentInfo]) -> String {
    let blocks = html_in::parse_message_blocks(html, attachments, &[]);
    html_in::blocks_to_markdown(&blocks)
}

/// Parses Teams HTML + Adaptive Cards into structured `RichBlock`s for styled `ratatui` rendering.
pub fn parse_message_blocks(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Vec<RichBlock> {
    html_in::parse_message_blocks(html, attachments, mentions)
}

/// Compiles user Markdown input + `@mention` list + uploaded reference attachments + inline images
/// + optional quote reply into a valid `OutgoingMessage` ready for Microsoft Graph.
pub fn compile_outgoing_markdown(
    markdown: &str,
    subject: Option<String>,
    mentions: Vec<MentionInfo>,
    attachments: Vec<UploadedReferenceAttachment>,
    inline_images: Vec<InlineImageUpload>,
    quote_reply_to: Option<QuotedReply>,
) -> OutgoingMessage {
    md_out::compile_outgoing_markdown(
        markdown,
        subject,
        mentions,
        attachments,
        inline_images,
        quote_reply_to,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_blocks_plain_and_markdown() {
        let html = r#"
            <blockquote itemtype="http://schema.skype.com/Reply" itemid="100"><strong>김철수</strong><br>이전 질문입니다</blockquote>
            <p>안녕하세요 <at id="0">홍길동</at>님, <b>배포</b> <i>일정</i> <s>취소</s> <u>공지</u> <code>v1.2</code> <a href="https://example.com">링크</a> 확인!</p>
            <h2>체크리스트</h2>
            <ul><li>빌드 확인</li><li>테스트 통과</li></ul>
            <pre><code class="language-rust">fn main() { println!("hi"); }</code></pre>
            <img src="https://graph.microsoft.com/img1">
            <attachment id="att-1"></attachment>
        "#;
        let attachments = vec![
            AttachmentInfo {
                id: "att-1".to_string(),
                content_type: "reference".to_string(),
                content_url: Some("https://contoso.sharepoint.com/spec.pdf".to_string()),
                name: Some("spec.pdf".to_string()),
                content_json: None,
                thumbnail_url: None,
            },
            AttachmentInfo {
                id: "card-unreferenced".to_string(),
                content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                content_url: None,
                name: Some("배포 알림".to_string()),
                content_json: Some(
                    r#"{
                        "type": "AdaptiveCard",
                        "body": [
                            {"type": "TextBlock", "text": "릴리즈 요약", "size": "Large", "weight": "Bolder"},
                            {"type": "FactSet", "facts": [{"title": "환경", "value": "Production"}]},
                            {"type": "ColumnSet", "columns": [
                                {"type": "Column", "items": [{"type": "TextBlock", "text": "컬럼 A"}]},
                                {"type": "Column", "items": [{"type": "TextBlock", "text": "컬럼 B"}]}
                            ]},
                            {"type": "Container", "items": [{"type": "TextBlock", "text": "정상 배포 완료"}]}
                        ],
                        "actions": [
                            {"type": "Action.OpenUrl", "title": "대시보드", "url": "https://dash.example.com"}
                        ]
                    }"#
                    .to_string(),
                ),
                thumbnail_url: None,
            },
        ];
        let mentions = vec![MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "홍길동".to_string(),
        }];

        let blocks = parse_message_blocks(html, &attachments, &mentions);
        assert!(matches!(blocks[0], RichBlock::Quote(_)));
        assert!(matches!(blocks[1], RichBlock::Paragraph(_)));
        assert!(matches!(blocks[2], RichBlock::Heading { level: 2, .. }));
        assert!(matches!(blocks[3], RichBlock::BulletList(_)));
        assert!(matches!(
            blocks[4],
            RichBlock::CodeBlock {
                lang: Some(_),
                ..
            }
        ));
        assert!(matches!(
            blocks[5],
            RichBlock::InlineImageChip { index: 0, .. }
        ));
        assert!(matches!(blocks[6], RichBlock::AttachmentChip { .. }));
        assert!(matches!(blocks[7], RichBlock::Card { .. }));

        if let RichBlock::Card {
            title,
            facts,
            body_lines,
            actions,
            ..
        } = &blocks[7]
        {
            assert_eq!(title.as_deref(), Some("릴리즈 요약"));
            assert_eq!(facts, &vec![("환경".to_string(), "Production".to_string())]);
            assert_eq!(
                body_lines,
                &vec![
                    "컬럼 A".to_string(),
                    "컬럼 B".to_string(),
                    "정상 배포 완료".to_string()
                ]
            );
            assert_eq!(
                actions,
                &vec![("대시보드".to_string(), "https://dash.example.com".to_string())]
            );
        }

        let plain = html_and_cards_to_plain(html, &attachments);
        assert!(plain.contains("@홍길동"));
        assert!(plain.contains("릴리즈 요약"));
        assert!(plain.contains("환경: Production"));
        assert!(plain.contains("[첨부: spec.pdf]"));
        assert!(!plain.contains("schema.skype.com"));

        let md = html_and_cards_to_markdown(html, &attachments);
        assert!(md.contains("**배포**"));
        assert!(md.contains("*일정*"));
        assert!(md.contains("~~취소~~"));
        assert!(md.contains("[링크](https://example.com)"));
        assert!(md.contains("```rust"));
        assert!(md.contains("## 체크리스트"));
    }

    #[test]
    fn test_o365_connector_card_and_columnset() {
        let att = AttachmentInfo {
            id: "c1".to_string(),
            content_type: "application/vnd.microsoft.teams.card.o365connector".to_string(),
            content_url: None,
            name: None,
            content_json: Some(
                r#"{
                    "@type": "MessageCard",
                    "title": "CI 빌드 실패",
                    "summary": "main 브랜치",
                    "sections": [{
                        "activityTitle": "GitHub Actions",
                        "facts": [{"name": "커밋", "value": "abc1234"}],
                        "text": "cargo test 실패"
                    }],
                    "potentialAction": [{
                        "name": "로그 보기",
                        "targets": [{"uri": "https://ci.example.com/1"}]
                    }]
                }"#
                .to_string(),
            ),
            thumbnail_url: None,
        };
        let blocks = parse_message_blocks("", &[att], &[]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                assert_eq!(title.as_deref(), Some("CI 빌드 실패"));
                assert_eq!(subtitle.as_deref(), Some("main 브랜치"));
                assert_eq!(facts, &vec![("커밋".to_string(), "abc1234".to_string())]);
                assert!(body_lines.contains(&"cargo test 실패".to_string()));
                assert_eq!(
                    actions,
                    &vec![("로그 보기".to_string(), "https://ci.example.com/1".to_string())]
                );
            }
            other => panic!("expected Card, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_outgoing_markdown_escaping_and_features() {
        let md = "안녕 @홍길동! `<script>alert(1)</script>`\n**굵게** *기울임* ~~취소~~ [문서](https://example.com?a=1&b=2)\n\n> 인용 첫줄\n> 인용 둘줄\n\n```rust\nlet x = 1 < 2;\n```\n\n<raw-html-must-escape>";
        let mentions = vec![MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "홍길동".to_string(),
        }];
        let attachments = vec![UploadedReferenceAttachment {
            id: "guid-1".to_string(),
            name: "report.pdf".to_string(),
            content_url: "https://sharepoint/report.pdf".to_string(),
            size: 1024,
            drive_item_id: None,
        }];
        let inline_images = vec![InlineImageUpload {
            temp_id: "1".to_string(),
            mime_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
            file_name: Some("shot.png".to_string()),
        }];
        let quote = Some(QuotedReply {
            msg_id: "m99".to_string(),
            author_name: "김<철>수".to_string(),
            preview_plain: "원본 & 내용".to_string(),
        });

        let out = compile_outgoing_markdown(
            md,
            Some("제목".to_string()),
            mentions,
            attachments,
            inline_images,
            quote,
        );

        assert!(out
            .body_html
            .starts_with("<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"m99\"><strong>김&lt;철&gt;수</strong><br>원본 &amp; 내용</blockquote>"));
        assert!(out.body_html.contains("<at id=\"0\">홍길동</at>"));
        assert!(out
            .body_html
            .contains("<code>&lt;script&gt;alert(1)&lt;/script&gt;</code>"));
        assert!(out.body_html.contains("<strong>굵게</strong>"));
        assert!(out.body_html.contains("<em>기울임</em>"));
        assert!(out.body_html.contains("<s>취소</s>"));
        assert!(out
            .body_html
            .contains("<a href=\"https://example.com?a=1&amp;b=2\">문서</a>"));
        assert!(out
            .body_html
            .contains("<blockquote>인용 첫줄<br>인용 둘줄</blockquote>"));
        assert!(out
            .body_html
            .contains("<pre><code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>"));
        assert!(out.body_html.contains("&lt;raw-html-must-escape&gt;"));
        assert!(!out.body_html.contains("<raw-html-must-escape>"));
        assert!(out
            .body_html
            .contains("<img src=\"../hostedContents/1/$value\">"));
        assert!(out
            .body_html
            .contains("<attachment id=\"guid-1\"></attachment>"));
    }
}
