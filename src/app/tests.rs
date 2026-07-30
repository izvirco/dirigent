
use super::{
    FrameTiming, FrameTimingSample, assistant_failure, composer_path_query, content_text,
    conversation_list_splice, directory_path_query, effective_settings_before_entry,
    entries_through_leaf, parse_available_model, parse_cached_draft_images, parse_context_usage,
    parse_entries, parse_message, parse_messages, reconcile_queued_messages, resolve_tilde_path,
    rpc_string_array, tool_expanded, tool_label, tool_result_detail, truncate_output, write_detail,
};
use crate::model::{Message, MessageRole};
use serde_json::json;
use std::{collections::VecDeque, path::PathBuf, time::Duration};

#[test]
fn finds_the_active_composer_file_mention() {
    let text = "please inspect @src/ui/com";
    let (range, query) = composer_path_query(text, text.len()).unwrap();

    assert_eq!(&text[range], "src/ui/com");
    assert_eq!(query, "src/ui/com");
    assert!(composer_path_query("email@example.com", 17).is_none());
    assert!(composer_path_query("@src/app.rs then", 16).is_none());
}

#[test]
fn searches_only_children_of_the_typed_directory() {
    let base = std::env::temp_dir().join(format!("dirigent-path-query-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    for name in ["alpha", "beta", "delta", "gamma", "izvir", "omega", "zeta"] {
        std::fs::create_dir_all(base.join(name)).unwrap();
    }
    std::fs::create_dir_all(base.join("alpha/nested")).unwrap();
    std::fs::write(base.join("not-a-directory"), "fixture").unwrap();
    let raw = base.join("pro");

    let (root, query) = directory_path_query(raw.to_str().unwrap()).unwrap();
    let children = super::directory_child_results(&base, "");

    assert_eq!(root, base);
    assert_eq!(query, "pro");
    assert_eq!(children.len(), 7);
    assert!(
        children
            .iter()
            .all(|path| { std::path::Path::new(path).parent() == Some(base.as_path()) })
    );
    assert_eq!(
        directory_path_query(&format!("{}/", base.display())),
        Some((base.clone(), String::new()))
    );
    assert!(directory_path_query(base.join("missing/pro").to_str().unwrap()).is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resolves_tilde_against_the_home_directory() {
    assert_eq!(
        resolve_tilde_path("~/work/project", std::path::Path::new("/home/test")),
        Some(PathBuf::from("/home/test/work/project"))
    );
    assert_eq!(
        resolve_tilde_path(r"~\work\project", std::path::Path::new("/home/test")),
        Some(std::path::Path::new("/home/test").join(r"work\project"))
    );
    assert_eq!(
        resolve_tilde_path("~", std::path::Path::new("/home/test")),
        Some(PathBuf::from("/home/test"))
    );
    assert!(resolve_tilde_path("~other/project", std::path::Path::new("/home/test")).is_none());
}

#[test]
fn fff_indexes_and_fuzzy_finds_project_files() {
    let base = std::env::temp_dir().join(format!("dirigent-fff-index-{}", std::process::id()));
    let source = base.join("src");
    std::fs::create_dir_all(source.join("ui")).unwrap();
    std::fs::write(source.join("project_setup.rs"), "// fixture").unwrap();
    std::fs::write(source.join("ui/composer.rs"), "// fixture").unwrap();
    let (events, _) = async_channel::unbounded();
    let picker =
        super::start_fuzzy_index(&base, false, super::FuzzyIndexReady::Project(0), events).unwrap();
    assert!(picker.wait_for_scan(Duration::from_secs(5)));

    let initial_results = super::fuzzy_file_results(&picker, "");
    let results = super::fuzzy_file_results(&picker, "prjsetup");

    assert!(
        initial_results
            .iter()
            .any(|path| path == "src/project_setup.rs")
    );
    assert!(results.iter().any(|path| path == "src/project_setup.rs"));
    picker.cancel();
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn summarizes_frame_performance_percentiles() {
    let samples = (1..=100)
        .map(|sample| FrameTimingSample {
            draw_ms: sample as f32,
            response_ms: Some(sample as f32 * 2.0),
            invalidations: 2,
        })
        .collect::<VecDeque<_>>();
    let summary = FrameTiming::summarize(&samples).unwrap();

    assert_eq!(summary.draw_average_ms, 50.5);
    assert_eq!(summary.draw_p99_ms, 99.0);
    assert_eq!(summary.draw_maximum_ms, 100.0);
    assert_eq!(summary.response_p99_ms, Some(198.0));
    assert_eq!(summary.invalidations_average, 2.0);
    assert_eq!(summary.sample_count, 100);
}

#[test]
fn tolerates_frames_without_response_timing() {
    let samples = VecDeque::from([FrameTimingSample {
        draw_ms: 3.0,
        response_ms: None,
        invalidations: 1,
    }]);

    assert_eq!(
        FrameTiming::summarize(&samples).unwrap().response_p99_ms,
        None
    );
}

#[test]
fn conversation_list_splice_removes_messages_missing_from_canonical_history() {
    let (old_range, new_item_count) = conversation_list_splice(30, false, 25, false);

    assert_eq!(old_range, 25..30);
    assert_eq!(new_item_count, 0);
}

#[test]
fn restores_cached_composer_images() {
    let images = parse_cached_draft_images(
        br#"[{"label":"image-3","mime_type":"image/png","data":"AQID"}]"#,
    )
    .unwrap();

    assert_eq!(images.len(), 1);
    assert_eq!(images[0].label, "image-3");
    assert_eq!(images[0].image.bytes.as_slice(), &[1, 2, 3]);
}

#[test]
fn extracts_text_blocks_from_pi_messages() {
    let value = json!([
        {"type":"thinking","thinking":"hidden"},
        {"type":"text","text":"hello"},
        {"type":"text","text":"world"}
    ]);
    assert_eq!(content_text(&value), "hello\nworld");
}

#[test]
fn restores_user_messages() {
    let message = parse_message(&json!({"role":"user","content":"do it"})).unwrap();
    assert_eq!(message.role, MessageRole::User);
    assert_eq!(message.text, "do it");
}

#[test]
fn restores_images_on_user_messages() {
    let message = parse_message(&json!({
        "role":"user",
        "content":[
            {"type":"text","text":"[image-1] inspect this"},
            {"type":"image","data":"AA==","mimeType":"image/png"}
        ]
    }))
    .unwrap();
    assert_eq!(message.text, "[image-1] inspect this");
    assert_eq!(message.images.len(), 1);
}

#[test]
fn tool_output_is_bounded() {
    assert!(truncate_output(&"x".repeat(5_000)).len() < 5_000);
}

#[test]
fn tool_labels_are_single_line_commands() {
    assert_eq!(
        tool_label("bash", &json!({"command":"cargo fmt\ncargo test"})),
        "cargo fmt cargo test"
    );
    assert_eq!(
        tool_label("read", &json!({"path":"src/app.rs"})),
        "read src/app.rs"
    );
}

#[test]
fn formats_write_content_as_added_lines() {
    assert_eq!(
        write_detail(&json!({"content":"fn main() {\n    run();\n}\n"})).as_deref(),
        Some("+ fn main() {\n+     run();\n+ }\n")
    );
    assert!(tool_expanded("write"));
}

#[test]
fn parses_context_usage() {
    let usage = parse_context_usage(&json!({
        "data": {
            "contextUsage": {
                "tokens": 60_000,
                "contextWindow": 200_000,
                "percent": 30
            }
        }
    }))
    .unwrap();

    assert_eq!(usage.used_tokens, 60_000);
    assert_eq!(usage.context_window, 200_000);
}

#[test]
fn parses_available_model_picker_options() {
    let model = parse_available_model(&json!({
        "provider":"openai-codex",
        "id":"gpt-5.6-sol",
        "name":"GPT-5.6 Sol",
        "reasoning":true,
        "thinkingLevelMap":{"xhigh":"xhigh","max":"max"}
    }))
    .unwrap();
    assert_eq!(model.provider, "openai-codex");
    assert_eq!(model.id, "gpt-5.6-sol");
    assert!(model.supports_xhigh);
    assert!(model.supports_max);
}

#[test]
fn tracks_rpc_queue_state_and_clears_delivered_messages() {
    let value = json!({
        "type":"queue_update",
        "steering":["second"],
        "followUp":["later"]
    });
    assert_eq!(rpc_string_array(&value, "steering"), ["second"]);
    assert_eq!(rpc_string_array(&value, "followUp"), ["later"]);

    let mut first = Message::new(MessageRole::User, "first");
    first.queued = true;
    let mut second = Message::new(MessageRole::User, "second");
    second.queued = true;
    let mut messages = vec![first, second];
    let accepted = reconcile_queued_messages(&mut messages, &["second".into()]);

    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].text, "first");
    assert!(!accepted[0].queued);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "second");
    assert!(messages[0].queued);

    let mut older = Message::new(MessageRole::User, "duplicate");
    older.queued = true;
    let mut newer = Message::new(MessageRole::User, "duplicate");
    newer.queued = true;
    let mut duplicates = vec![older, newer];
    let accepted = reconcile_queued_messages(&mut duplicates, &["duplicate".into()]);
    assert_eq!(accepted.len(), 1);
    assert!(!accepted[0].queued);
    assert_eq!(duplicates.len(), 1);
    assert!(duplicates[0].queued);
}

#[test]
fn surfaces_assistant_failures_from_pi_messages() {
    let messages = parse_messages(&[json!({
        "role":"assistant",
        "content":[],
        "stopReason":"error",
        "errorMessage":"This model does not support image input"
    })]);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, MessageRole::Error);
    assert_eq!(messages[0].text, "This model does not support image input");
}

#[test]
fn reports_output_limit_and_aborted_assistant_messages() {
    assert_eq!(
        assistant_failure(&json!({"role":"assistant", "stopReason":"length"})).as_deref(),
        Some("The model reached its maximum output token limit; the response may be incomplete.")
    );
    assert_eq!(
        assistant_failure(&json!({
            "role":"assistant",
            "stopReason":"aborted",
            "errorMessage":"Request was aborted"
        }))
        .as_deref(),
        Some("Operation aborted.")
    );
}

#[test]
fn restores_thinking_blocks_in_content_order() {
    let messages = parse_messages(&[
        json!({
            "role":"assistant",
            "content":[
                {"type":"thinking","thinking":"I should inspect the file."},
                {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"src/app.rs"}},
                {"type":"text","text":"Done."}
            ]
        }),
        json!({"role":"toolResult","toolCallId":"call-1","toolName":"read","content":"file"}),
    ]);

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, MessageRole::Thinking);
    assert_eq!(messages[0].text, "I should inspect the file.");
    assert_eq!(messages[1].role, MessageRole::Tool);
    assert_eq!(messages[2].role, MessageRole::Assistant);
}

#[test]
fn restores_tool_commands_with_collapsed_output() {
    let messages = parse_messages(&[
        json!({
            "role":"assistant",
            "content":[{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"cargo fmt\ncargo test"}}]
        }),
        json!({"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":"ok"}),
    ]);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "cargo fmt cargo test");
    assert_eq!(messages[0].detail.as_deref(), Some("ok"));
    assert!(!messages[0].expanded);
}

#[test]
fn restores_write_content_instead_of_success_message() {
    let messages = parse_messages(&[
        json!({
            "role":"assistant",
            "content":[{"type":"toolCall","id":"call-1","name":"write","arguments":{
                "path":"src/main.rs",
                "content":"fn main() {}\n"
            }}]
        }),
        json!({
            "role":"toolResult",
            "toolCallId":"call-1",
            "toolName":"write",
            "content":"Successfully wrote 13 bytes to src/main.rs."
        }),
    ]);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "write src/main.rs");
    assert_eq!(messages[0].detail.as_deref(), Some("+ fn main() {}\n"));
    assert!(messages[0].expanded);
}

#[test]
fn restores_edit_diff_instead_of_success_message() {
    let messages = parse_messages(&[
        json!({
            "role":"assistant",
            "content":[{"type":"toolCall","id":"call-1","name":"edit","arguments":{"path":"src/app.rs","edits":[]}}]
        }),
        json!({
            "role":"toolResult",
            "toolCallId":"call-1",
            "toolName":"edit",
            "content":"Successfully replaced 1 block(s) in src/app.rs.",
            "details":{"diff":" 9 before\n-10 old\n+10 new\n 11 after"}
        }),
    ]);

    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].detail.as_deref(),
        Some("  9 before\n- 10 old\n+ 10 new\n  11 after")
    );
    assert!(messages[0].expanded);
}

#[test]
fn restores_complete_active_history_across_compaction() {
    let entries = vec![
        json!({
            "type":"message", "id":"user-old", "parentId":null,
            "message":{"role":"user","content":"old prompt"}
        }),
        json!({
            "type":"message", "id":"assistant-old", "parentId":"user-old",
            "message":{"role":"assistant","content":[{"type":"text","text":"old answer"}]}
        }),
        json!({
            "type":"compaction", "id":"compact-1", "parentId":"assistant-old",
            "summary":"summary of old work", "tokensBefore":100000
        }),
        json!({
            "type":"message", "id":"user-new", "parentId":"compact-1",
            "message":{"role":"user","content":"new prompt"}
        }),
        json!({
            "type":"message", "id":"other-branch", "parentId":"user-old",
            "message":{"role":"user","content":"abandoned prompt"}
        }),
    ];

    let messages = parse_entries(&entries, Some("user-new"));

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].text, "old prompt");
    assert_eq!(messages[1].text, "old answer");
    assert!(messages[2].is_compaction());
    assert_eq!(messages[2].detail.as_deref(), Some("summary of old work"));
    assert_eq!(messages[3].text, "new prompt");
    assert!(
        messages
            .iter()
            .all(|message| message.text != "abandoned prompt")
    );
}

#[test]
fn extracts_only_the_branch_written_to_a_forked_session() {
    let entries = vec![
        json!({"type":"message", "id":"root", "parentId":null}),
        json!({"type":"message", "id":"active", "parentId":"root"}),
        json!({"type":"message", "id":"abandoned", "parentId":"root"}),
    ];

    assert_eq!(
        entries_through_leaf(&entries, Some("active"))
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str()))
            .collect::<Vec<_>>(),
        ["root", "active"]
    );
    assert!(entries_through_leaf(&entries, None).is_empty());
}

#[test]
fn canonical_messages_keep_their_pi_entry_ids() {
    let entries = vec![
        json!({
            "type":"message", "id":"user-1", "parentId":null,
            "message":{"role":"user","content":"prompt"}
        }),
        json!({
            "type":"message", "id":"assistant-1", "parentId":"user-1",
            "message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"plan"},
                {"type":"text","text":"answer"}
            ]}
        }),
    ];

    let messages = parse_entries(&entries, Some("assistant-1"));

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].entry_id.as_deref(), Some("user-1"));
    assert_eq!(messages[1].entry_id.as_deref(), Some("assistant-1"));
    assert_eq!(messages[2].entry_id.as_deref(), Some("assistant-1"));
}

#[test]
fn assistant_text_from_separate_entries_stays_separate() {
    let entries = vec![
        json!({
            "type":"message", "id":"assistant-1", "parentId":null,
            "message":{"role":"assistant","content":[{"type":"text","text":"first"}]}
        }),
        json!({
            "type":"message", "id":"assistant-2", "parentId":"assistant-1",
            "message":{"role":"assistant","content":[{"type":"text","text":"second"}]}
        }),
    ];

    let messages = parse_entries(&entries, Some("assistant-2"));

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text, "first");
    assert_eq!(messages[1].text, "second");
}

#[test]
fn restores_model_and_thinking_effective_before_edited_message() {
    let entries = vec![
        json!({
            "type":"model_change", "id":"model-1", "parentId":null,
            "provider":"openai", "modelId":"gpt-5"
        }),
        json!({
            "type":"thinking_level_change", "id":"thinking-1", "parentId":"model-1",
            "thinkingLevel":"high"
        }),
        json!({
            "type":"message", "id":"user-1", "parentId":"thinking-1",
            "message":{"role":"user","content":"prompt"}
        }),
    ];

    assert_eq!(
        effective_settings_before_entry(&entries, "user-1", "fallback/model".into(), "off".into(),),
        ("openai/gpt-5".into(), "high".into())
    );
}

#[test]
fn edit_errors_still_show_the_error_message() {
    let result = json!({
        "content":"oldText was not found",
        "details":{"diff":"-old\n+new"}
    });
    assert_eq!(
        tool_result_detail("edit", &result, true).as_deref(),
        Some("oldText was not found")
    );
}
