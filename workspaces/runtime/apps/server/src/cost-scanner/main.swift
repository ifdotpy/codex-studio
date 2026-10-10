import Darwin
import Foundation

private struct Arguments {
    let home: URL
    let cache: URL
}

private struct CostModelBreakdown: Encodable {
    let modelName: String
    let costUSD: Double?
    let totalTokens: Int?

    private enum CodingKeys: String, CodingKey {
        case modelName
        case costUSD = "cost"
        case totalTokens
    }
}

private struct CostDailyEntry: Encodable {
    let date: String
    let inputTokens: Int?
    let outputTokens: Int?
    let cacheReadTokens: Int?
    let cacheCreationTokens: Int?
    let totalTokens: Int?
    let costUSD: Double?
    let modelsUsed: [String]?
    let modelBreakdowns: [CostModelBreakdown]?

    private enum CodingKeys: String, CodingKey {
        case date
        case inputTokens
        case outputTokens
        case cacheReadTokens
        case cacheCreationTokens
        case totalTokens
        case costUSD = "totalCost"
        case modelsUsed
        case modelBreakdowns
    }
}

private struct CostTotals: Encodable {
    let inputTokens: Int?
    let outputTokens: Int?
    let cacheReadTokens: Int?
    let cacheCreationTokens: Int?
    let totalTokens: Int?
    let totalCostUSD: Double?

    private enum CodingKeys: String, CodingKey {
        case inputTokens
        case outputTokens
        case cacheReadTokens
        case cacheCreationTokens
        case totalTokens
        case totalCostUSD = "totalCost"
    }
}

private struct CostPayload: Encodable {
    let provider: String
    let source: String
    let updatedAt: Date
    let currencyCode: String
    let sessionTokens: Int?
    let sessionCostUSD: Double?
    let historyDays: Int
    let last30DaysTokens: Int?
    let last30DaysCostUSD: Double?
    let daily: [CostDailyEntry]
    let totals: CostTotals?
    let historyCoverageIsEstablished: Bool
    let coverage: [String: Int]
}

private enum HelperError: LocalizedError {
    case usage(String)
    case scan(String)

    var errorDescription: String? {
        switch self {
        case let .usage(message), let .scan(message): message
        }
    }
}

private func usage() -> Never {
    FileHandle.standardError.write(
        Data("usage: codex-cost-scanner --home ABSOLUTE_HOME --cache ABSOLUTE_CACHE\n".utf8))
    exit(2)
}

private func parseArguments() throws -> Arguments {
    var home: String?
    var cache: String?
    var index = 1
    while index < CommandLine.arguments.count {
        switch CommandLine.arguments[index] {
        case "--help", "-h": usage()
        case "--home":
            index += 1
            guard index < CommandLine.arguments.count else { throw HelperError.usage("--home needs a value") }
            home = CommandLine.arguments[index]
        case "--cache":
            index += 1
            guard index < CommandLine.arguments.count else { throw HelperError.usage("--cache needs a value") }
            cache = CommandLine.arguments[index]
        default:
            throw HelperError.usage("unknown option: \(CommandLine.arguments[index])")
        }
        index += 1
    }

    guard let home, let cache,
          home.hasPrefix("/"), cache.hasPrefix("/")
    else {
        throw HelperError.usage("--home and --cache must be absolute paths")
    }

    let homeURL = URL(fileURLWithPath: home, isDirectory: true)
        .standardizedFileURL
        .resolvingSymlinksInPath()
    var isDirectory: ObjCBool = false
    guard FileManager.default.fileExists(atPath: homeURL.path, isDirectory: &isDirectory),
          isDirectory.boolValue
    else {
        throw HelperError.usage("Codex home is not a directory")
    }

    let cacheURL = URL(fileURLWithPath: cache, isDirectory: true).standardizedFileURL
    try FileManager.default.createDirectory(at: cacheURL, withIntermediateDirectories: true)
    chmod(cacheURL.path, 0o700)

    return Arguments(home: homeURL, cache: cacheURL)
}

private func pathIsInside(_ child: URL, base: URL) -> Bool {
    let basePath = base.resolvingSymlinksInPath().standardizedFileURL.path
    let childPath = child.resolvingSymlinksInPath().standardizedFileURL.path
    return childPath == basePath || childPath.hasPrefix(basePath + "/")
}

private func copyInstalledPricingIfNewer(to cache: URL) {
    // CodexBar keeps model pricing separately from token usage. Reuse that
    // non-secret catalog, but never reuse the shared cost-usage cache.
    guard let globalRoot = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first?
        .appendingPathComponent("CodexBar", isDirectory: true)
        .appendingPathComponent("model-pricing", isDirectory: true)
    else { return }
    let global = globalRoot.appendingPathComponent("models-dev-v1.json", isDirectory: false)
    let scopedRoot = cache.appendingPathComponent("model-pricing", isDirectory: true)
    let scoped = scopedRoot.appendingPathComponent("models-dev-v1.json", isDirectory: false)
    guard FileManager.default.fileExists(atPath: global.path) else { return }
    let globalDate = (try? FileManager.default.attributesOfItem(atPath: global.path)[.modificationDate]) as? Date
    let scopedDate = (try? FileManager.default.attributesOfItem(atPath: scoped.path)[.modificationDate]) as? Date
    guard scopedDate == nil || (globalDate != nil && globalDate! > scopedDate!) else { return }
    guard let data = try? Data(contentsOf: global) else { return }
    do {
        try FileManager.default.createDirectory(at: scopedRoot, withIntermediateDirectories: true)
        try data.write(to: scoped, options: [.atomic])
        chmod(scoped.path, 0o600)
    } catch {
        // Built-in v0.37.2 rates remain available when the catalog is absent.
    }
}

private func scan(_ arguments: Arguments) throws -> CostPayload {
    let sessionsURL = arguments.home.appendingPathComponent("sessions", isDirectory: true)
    let archivedURL = arguments.home.appendingPathComponent("archived_sessions", isDirectory: true)
    guard pathIsInside(sessionsURL, base: arguments.home),
          pathIsInside(archivedURL, base: arguments.home)
    else {
        throw HelperError.scan("Codex session roots leave the selected home")
    }

    // The upstream scanner defaults this database to ~/.codex/logs_2.sqlite.
    // Pin it to the selected profile so no ambient account can contribute.
    let profileTraceURL = arguments.home.appendingPathComponent("logs_2.sqlite", isDirectory: false)
    let traceURL = pathIsInside(profileTraceURL, base: arguments.home)
        ? profileTraceURL
        : arguments.home.appendingPathComponent(".codex-cost-scanner-no-trace.sqlite", isDirectory: false)
    copyInstalledPricingIfNewer(to: arguments.cache)
    let now = Date()
    let since = Calendar.current.date(byAdding: .day, value: -29, to: now) ?? now
    var options = CostUsageScanner.Options(
        codexSessionsRoot: sessionsURL,
        cacheRoot: arguments.cache,
        codexTraceDatabaseURL: traceURL)
    // The parent reader owns the account-level cadence. Let every helper
    // invocation inspect the incremental cache without forcing a full scan.
    options.refreshMinIntervalSeconds = 0
    let report: CostUsageDailyReport
    do {
        report = try CostUsageScanner.loadDailyReportCancellable(
            provider: .codex,
            since: since,
            until: now,
            now: now,
            options: options,
            checkCancellation: nil)
    } catch {
        throw HelperError.scan("Codex log scan failed: \(error.localizedDescription)")
    }

    let daily = report.data.map { entry in
        CostDailyEntry(
            date: entry.date,
            inputTokens: entry.inputTokens,
            outputTokens: entry.outputTokens,
            cacheReadTokens: entry.cacheReadTokens,
            cacheCreationTokens: entry.cacheCreationTokens,
            totalTokens: entry.totalTokens,
            costUSD: entry.costUSD,
            modelsUsed: entry.modelsUsed,
            modelBreakdowns: entry.modelBreakdowns?.map {
                CostModelBreakdown(
                    modelName: $0.modelName,
                    costUSD: $0.costUSD,
                    totalTokens: $0.totalTokens)
            })
    }
    let dayKey = CostUsageScanner.CostUsageDayRange.dayKey(from: now)
    let today = report.data.first(where: { $0.date == dayKey })
    var priced = 0
    var unpriced = 0
    for entry in report.data {
        let breakdowns = entry.modelBreakdowns ?? []
        priced += breakdowns.reduce(into: 0) { count, breakdown in
            if breakdown.costUSD != nil { count += 1 }
        }
        unpriced += breakdowns.reduce(into: 0) { count, breakdown in
            if breakdown.costUSD == nil { count += 1 }
        }
        let coveredModels = Set(breakdowns.map(\.modelName))
        unpriced += (entry.modelsUsed ?? []).filter { !coveredModels.contains($0) }.count
    }

    let totals = report.summary.map {
        CostTotals(
            inputTokens: $0.totalInputTokens,
            outputTokens: $0.totalOutputTokens,
            cacheReadTokens: $0.cacheReadTokens,
            cacheCreationTokens: $0.cacheCreationTokens,
            totalTokens: $0.totalTokens,
            totalCostUSD: $0.totalCostUSD)
    }
    return CostPayload(
        provider: "codex",
        source: "local",
        updatedAt: now,
        currencyCode: "USD",
        sessionTokens: today?.totalTokens,
        sessionCostUSD: today?.costUSD,
        historyDays: 30,
        last30DaysTokens: report.summary?.totalTokens,
        last30DaysCostUSD: report.summary?.totalCostUSD,
        daily: daily,
        totals: totals,
        historyCoverageIsEstablished: false,
        coverage: ["priced": priced, "unpriced": unpriced])
}

do {
    let arguments = try parseArguments()
    let payload = try scan(arguments)
    let encoder = JSONEncoder()
    encoder.dateEncodingStrategy = .iso8601
    encoder.outputFormatting = [.sortedKeys]
    print(String(decoding: try encoder.encode([payload]), as: UTF8.self))
} catch {
    FileHandle.standardError.write(Data("\(error.localizedDescription)\n".utf8))
    exit(1)
}
