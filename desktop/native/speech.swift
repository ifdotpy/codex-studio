import Foundation
import Speech
import AVFoundation

func emit(_ value: [String: Any], code: Int32 = 0) -> Never {
    let data = try! JSONSerialization.data(withJSONObject: value)
    print(String(data: data, encoding: .utf8)!)
    exit(code)
}
if CommandLine.arguments.contains("--check") {
    emit(["provider": "macOS Speech", "helperReady": true, "onDeviceOnly": true])
}
guard CommandLine.arguments.count == 3 else { emit(["error": "Expected a WAV file and a language."], code: 1) }
let fileURL = URL(fileURLWithPath: CommandLine.arguments[1])
let locale = Locale(identifier: CommandLine.arguments[2])
guard let recognizer = SFSpeechRecognizer(locale: locale), recognizer.supportsOnDeviceRecognition else {
    emit(["error": "On-device speech is unavailable for this language. Enable Dictation and download the language in macOS settings, then retry."], code: 1)
}
var permission: SFSpeechRecognizerAuthorizationStatus?
SFSpeechRecognizer.requestAuthorization { permission = $0 }
let permissionDeadline = Date().addingTimeInterval(120)
while permission == nil && Date() < permissionDeadline { RunLoop.current.run(until: Date().addingTimeInterval(0.05)) }
guard permission == .authorized else { emit(["error": "Speech recognition permission was not granted. Allow Codex Studio in macOS Privacy & Security, then retry."], code: 1) }
do {
    let source = try AVAudioFile(forReading: fileURL)
    let segmentFrames = AVAudioFrameCount(source.processingFormat.sampleRate * 50)
    var parts: [String] = []
    while source.framePosition < source.length {
        guard let buffer = AVAudioPCMBuffer(pcmFormat: source.processingFormat, frameCapacity: segmentFrames) else { throw NSError(domain: "Audio", code: 1, userInfo: [NSLocalizedDescriptionKey: "Cannot allocate an audio segment."]) }
        try source.read(into: buffer, frameCount: segmentFrames)
        let segment = fileURL.deletingLastPathComponent().appendingPathComponent("segment-\(UUID().uuidString).caf")
        do { let output = try AVAudioFile(forWriting: segment, settings: source.processingFormat.settings); try output.write(from: buffer) }
        defer { try? FileManager.default.removeItem(at: segment) }
        let request = SFSpeechURLRecognitionRequest(url: segment)
        request.requiresOnDeviceRecognition = true
        request.shouldReportPartialResults = false
        var final: String?, failure: Error?
        let task = recognizer.recognitionTask(with: request) { result, error in
            if let result = result, result.isFinal { final = result.bestTranscription.formattedString }
            if let error = error { failure = error }
        }
        let deadline = Date().addingTimeInterval(120)
        while final == nil && failure == nil && Date() < deadline { RunLoop.current.run(until: Date().addingTimeInterval(0.05)) }
        task.cancel()
        if let failure = failure { throw failure }
        guard let final = final else { throw NSError(domain: "Speech", code: 2, userInfo: [NSLocalizedDescriptionKey: "Speech recognition timed out. The recording is saved; retry transcription."]) }
        parts.append(final)
    }
    let text = parts.joined(separator: " ").trimmingCharacters(in: .whitespacesAndNewlines)
    if text.isEmpty { throw NSError(domain: "Speech", code: 3, userInfo: [NSLocalizedDescriptionKey: "No speech was recognized. Play the recording before retrying."]) }
    emit(["text": text, "provider": "macOS Speech", "onDevice": true])
} catch { emit(["error": error.localizedDescription], code: 1) }
