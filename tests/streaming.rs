use serde_json::json;
use werk_worker::providers::*;

#[test]
fn backend_errors_survive_arbitrary_sse_boundaries() {
    let message = "llama-server returned incomplete tool-call arguments: Grüße";
    for payload in [
        json!({"error":{"message":message,"type":"server_error","code":null}}),
        json!({"error":message}),
        json!({"error":{"message":message},"choices":[]}),
    ] {
        let wire = format!("event: error\ndata: {payload}\n\ndata: [DONE]\n\n");
        for size in 1..31 {
            let mut parser = SseDecoder::new(65536);
            let error = wire
                .as_bytes()
                .chunks(size)
                .find_map(|bytes| parser.push(bytes).err())
                .expect("backend error must terminate decoding");
            assert_eq!(error.code, "provider_error");
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
        assert_eq!(decode_completion(&payload).unwrap_err().message, message);
    }
    let error = decode_completion(&json!({"error":{}})).unwrap_err();
    assert_eq!(error.code, "provider_error");
    assert!(!error.message.is_empty());
}

#[test]
fn sse_arbitrary_byte_boundaries_utf8_tools_usage_and_nonstream_equivalence() {
    let chunks = [
        json!({"choices":[{"index":0,"delta":{"content":"Grüße ","tool_calls":[{"index":0,"id":"ca","function":{"name":"files.","arguments":"{\"pa"}},{"index":1,"id":"sec","function":{"name":"task.","arguments":"{"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{"content":"世界","tool_calls":[{"index":1,"id":"ond","function":{"name":"plan","arguments":"\"steps\":[]}"}},{"index":0,"id":"ll","function":{"name":"read","arguments":"th\":\"ä.txt\"}"}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":9}}),
    ];
    let mut wire = chunks
        .iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect::<String>();
    wire.push_str("data: [DONE]\n\n");
    for size in 1..31 {
        let mut parser = SseDecoder::new(65536);
        let mut assembler = ResponseAssembler::default();
        for bytes in wire.as_bytes().chunks(size) {
            for delta in parser.push(bytes).unwrap() {
                assembler.push(&delta, 65536).unwrap();
            }
        }
        parser.finish().unwrap();
        let (message, usage, _) = assembler.finish().unwrap();
        assert_eq!(message.content, "Grüße 世界");
        assert_eq!(message.tool_calls[0].id, "call");
        assert_eq!(message.tool_calls[1].id, "second");
        assert_eq!(message.tool_calls[0].arguments["path"], "ä.txt");
        assert_eq!(usage.unwrap().input_tokens, Some(42));
        let value = json!({"choices":[{"message":{"content":"Grüße 世界","tool_calls":[{"id":"call","function":{"name":"files.read","arguments":"{\"path\":\"ä.txt\"}"}},{"id":"second","function":{"name":"task.plan","arguments":"{\"steps\":[]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":9}});
        let mut other = ResponseAssembler::default();
        for delta in decode_completion(&value).unwrap() {
            other.push(&delta, 65536).unwrap();
        }
        assert_eq!(other.finish().unwrap().0, message);
    }
}
#[test]
fn incomplete_and_duplicate_calls_are_not_executable() {
    let mut parser = SseDecoder::new(1000);
    parser.push(b"data: {\"choices\":[]}\n\n").unwrap();
    assert_eq!(parser.finish().unwrap_err().code, "interrupted_stream");
    let mut a = ResponseAssembler::default();
    for index in 0..2 {
        a.push(
            &ModelDelta::Tool {
                index,
                id: "same".into(),
                name: "task.plan".into(),
                arguments: "{}".into(),
            },
            1000,
        )
        .unwrap();
    }
    a.push(
        &ModelDelta::Finish {
            reason: "tool_calls".into(),
        },
        1000,
    )
    .unwrap();
    a.push(&ModelDelta::Done, 1000).unwrap();
    assert_eq!(a.finish().unwrap_err().code, "invalid_tool_calls");
}

#[test]
fn credentials_are_redacted_even_across_text_and_tool_fragments() {
    let mut redactor = StreamRedactor::new(Some("SECRET-123".into()));
    let mut assembler = ResponseAssembler::default();
    let mut visible = String::new();
    for delta in [
        ModelDelta::Text {
            text: "prefix SEC".into(),
        },
        ModelDelta::Text {
            text: "RET-123 suffix".into(),
        },
        ModelDelta::Tool {
            index: 0,
            id: "c".into(),
            name: "tool".into(),
            arguments: "{\"value\":\"SEC".into(),
        },
        ModelDelta::Tool {
            index: 0,
            id: "".into(),
            name: "".into(),
            arguments: "RET-123\"}".into(),
        },
        ModelDelta::Finish {
            reason: "tool_calls".into(),
        },
        ModelDelta::Done,
    ] {
        for clean in redactor.push(delta) {
            visible.push_str(&serde_json::to_string(&clean).unwrap());
            assembler.push(&clean, 1000).unwrap();
        }
    }
    let (message, _, _) = assembler.finish().unwrap();
    assert_eq!(message.content, "prefix [redacted] suffix");
    assert_eq!(message.tool_calls[0].arguments["value"], "[redacted]");
    assert!(!visible.contains("SECRET-123"));
}

#[test]
fn server_output_limit_rejects_fragments_without_blind_transport_retry() {
    let mut assembler = ResponseAssembler::default();
    assembler
        .push(
            &ModelDelta::Tool {
                index: 0,
                id: "call".into(),
                name: "files.write".into(),
                arguments: "{\"path\":".into(),
            },
            65536,
        )
        .unwrap();
    assembler
        .push(
            &ModelDelta::Finish {
                reason: "length".into(),
            },
            65536,
        )
        .unwrap();
    assembler.push(&ModelDelta::Done, 65536).unwrap();
    let error = assembler.finish().unwrap_err();
    assert_eq!(error.code, "model_output_limit");
    assert!(!error.retryable);
}
