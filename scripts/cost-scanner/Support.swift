import Foundation

// This file supplies the small CodexBarCore surface used by the vendored cost
// scanner. It deliberately contains no provider fetchers, credentials, or
// network code.

enum UsageProvider: String, CaseIterable, Sendable, Codable {
    case codex
    case openai
    case azureopenai
    case claude
    case cursor
    case opencode
    case opencodego
    case alibaba
    case alibabatokenplan
    case factory
    case gemini
    case antigravity
    case copilot
    case devin
    case zai
    case minimax
    case manus
    case kimi
    case kilo
    case kiro
    case vertexai
    case augment
    case jetbrains
    case kimik2
    case moonshot
    case amp
    case t3chat
    case ollama
    case synthetic
    case warp
    case openrouter
    case elevenlabs
    case windsurf
    case zed
    case perplexity
    case mimo
    case doubao
    case abacus
    case mistral
    case deepseek
    case codebuff
    case crof
    case venice
    case commandcode
    case stepfun
    case bedrock
    case grok
    case groq
    case llmproxy
    case litellm
    case deepgram
    case poe
    case chutes
}

enum LogCategories {
    static let tokenCost = "token-cost"
}

struct CostScannerLogger: Sendable {
    func warning(_ message: @autoclosure () -> String, metadata: [String: String]? = nil) {}
}

enum CodexBarLog {
    static func logger(_ category: String) -> CostScannerLogger {
        CostScannerLogger()
    }
}
