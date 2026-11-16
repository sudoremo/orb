import Foundation

extension Error {
    /// What went wrong, in the core's words. The generated `OrbError`
    /// describes itself as Swift source (`OrbError.ImapError(message: …)`),
    /// which no one should be shown.
    public var message: String {
        guard let error = self as? OrbError else { return localizedDescription }
        switch error {
        case .ConfigError(let message), .DatabaseError(let message), .ImapError(let message),
             .SyncError(let message), .SendError(let message), .NotFound(let message),
             .InvalidInput(let message):
            return message
        case .ConfigMissing(let path):
            return "There is no configuration at \(path)."
        }
    }
}
