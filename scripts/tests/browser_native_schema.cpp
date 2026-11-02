// Opt-in regression against the installed llama.cpp grammar AND XML parser.
#include "parsers/parsers.h"
#include "llama-grammar.h"
#include <iostream>
#include <stdexcept>

static bool accepts(const std::string & grammar, const std::string & text) {
    auto * g = llama_grammar_init_impl(nullptr, grammar.c_str(), "root", false, nullptr, 0, nullptr, 0);
    if (!g) throw std::runtime_error("Native grammar could not be built");
    bool ok = true;
    // Fixtures use ASCII wire values; no tokenizer or model weights required.
    for (unsigned char c : text) {
        llama_grammar_accept(g, c);
        if (llama_grammar_get_stacks(g).empty()) { ok = false; break; }
    }
    bool complete = false;
    for (const auto & stack : llama_grammar_get_stacks(g)) complete |= stack.empty();
    llama_grammar_free_impl(g);
    return ok && complete;
}

int main() {
    try {
        const auto input = json::parse(std::string((std::istreambuf_iterator<char>(std::cin)), {}));
        common_chat_template tmpl(input.at("template"), "", "<|im_end|>");
        for (const auto & test : input.at("cases")) {
            const auto function = test.at("function");
            const std::string name = function.at("name");
            size_t parameters = 0;
            foreach_parameter(function, [&](size_t, const common_chat_schema_property &, const common_chat_schema_document_ptr &) { parameters++; });
            if (parameters != function.at("parameters").at("properties").size()) {
                throw std::runtime_error(name + ": native XML parser lost parameters");
            }
            autoparser::generation_params inputs;
            inputs.messages = json::array({{{"role", "user"}, {"content", "Use the browser."}}});
            inputs.tools = json::array({{{"type", "function"}, {"function", function}}});
            inputs.enable_thinking = false;
            inputs.add_generation_prompt = true;
            inputs.parallel_tool_calls = false;
            const auto params = common_chat_params_init_qwen3_coder(tmpl, inputs);
            std::string output = "<tool_call>\n<function=" + name + ">\n";
            for (const auto & item : test.at("arguments").items()) {
                const auto & value = item.value();
                output += "<parameter=" + item.key() + ">\n" +
                    (value.is_string() ? value.get<std::string>() : value.dump()) + "\n</parameter>\n";
            }
            output += "</function>\n</tool_call>";
            if (!accepts(params.grammar, output)) {
                throw std::runtime_error(name + ": native grammar rejects valid arguments");
            }
            const auto parsed = common_chat_peg_parse(params.parser, common_chat_input(output), false, common_chat_parser_params(params));
            if (parsed.tool_calls.size() != 1 || parsed.tool_calls[0].name != name ||
                json::parse(parsed.tool_calls[0].arguments) != test.at("arguments")) {
                throw std::runtime_error(name + ": native round-trip changed arguments");
            }
            std::cout << test.at("label").get<std::string>() << ": grammar and XML round-trip passed\n";
        }
    } catch (const std::exception & error) {
        std::cerr << error.what() << "\n";
        return 1;
    }
}
