import Foundation

/// Orb's core, reached from Swift without blocking the main thread.
///
/// Every call into the core blocks for as long as it takes, a server round
/// trip included, so each one runs on a queue of its own rather than on the
/// main actor or Swift's cooperative pool.
public final class OrbService: Sendable {
    public let orb: Orb
    private let queue = DispatchQueue(label: "orb.core", qos: .userInitiated, attributes: .concurrent)

    public init(configPath: String) async throws {
        orb = try await Self.offload(on: queue) { try Orb.open(configPath: configPath) }
    }

    /// Run `body` against the core off the main thread.
    public func run<T: Sendable>(_ body: @escaping @Sendable (Orb) throws -> T) async throws -> T {
        let orb = orb
        return try await Self.offload(on: queue) { try body(orb) }
    }

    /// The task uploader's rounds, as they finish (todo-spec §23).
    public func taskSyncEvents() -> AsyncStream<TaskSyncEvent> {
        let (stream, continuation) = AsyncStream.makeStream(of: TaskSyncEvent.self, bufferingPolicy: .bufferingNewest(1))
        orb.startTaskUploads(observer: StreamObserver(continuation: continuation))
        return stream
    }

    private static func offload<T: Sendable>(
        on queue: DispatchQueue,
        _ body: @escaping @Sendable () throws -> T
    ) async throws -> T {
        try await withCheckedThrowingContinuation { continuation in
            queue.async { continuation.resume(with: Result { try body() }) }
        }
    }
}

private final class StreamObserver: TaskSyncObserver, Sendable {
    let continuation: AsyncStream<TaskSyncEvent>.Continuation

    init(continuation: AsyncStream<TaskSyncEvent>.Continuation) {
        self.continuation = continuation
    }

    func onTaskSync(event: TaskSyncEvent) {
        continuation.yield(event)
    }
}
