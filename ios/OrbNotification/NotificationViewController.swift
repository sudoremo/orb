import SwiftUI
import UIKit
import UserNotifications
import UserNotificationsUI

/// A reminder pressed open: the task with its steps, so the next step can be
/// read without unlocking. Drawn from the notification's own payload, which
/// the app filled when it scheduled it; the extension never opens the
/// database.
final class NotificationViewController: UIViewController, UNNotificationContentExtension {
    private var host: UIHostingController<ReminderCard>?

    func didReceive(_ notification: UNNotification) {
        let content = notification.request.content
        let card = ReminderCard(
            title: content.title,
            when: content.body,
            steps: (content.userInfo["steps"] as? [[String: Any]] ?? []).compactMap { step in
                guard let title = step["title"] as? String else { return nil }
                return ReminderCard.Step(title: title, done: step["done"] as? Bool ?? false)
            }
        )
        host?.view.removeFromSuperview()
        let host = UIHostingController(rootView: card)
        host.view.backgroundColor = .clear
        addChild(host)
        host.view.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(host.view)
        NSLayoutConstraint.activate([
            host.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            host.view.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            host.view.topAnchor.constraint(equalTo: view.topAnchor),
            host.view.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])
        host.didMove(toParent: self)
        self.host = host
        preferredContentSize = host.sizeThatFits(in: CGSize(width: view.bounds.width, height: .greatestFiniteMagnitude))
    }
}

struct ReminderCard: View {
    struct Step: Hashable {
        let title: String
        let done: Bool
    }

    let title: String
    let when: String
    let steps: [Step]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            Text(when).font(.subheadline).foregroundStyle(.secondary)
            if !steps.isEmpty {
                let next = steps.firstIndex { !$0.done }
                Text("Steps · \(steps.filter(\.done).count) of \(steps.count)")
                    .font(.caption.bold())
                    .foregroundStyle(.secondary)
                    .padding(.top, 4)
                ForEach(Array(steps.enumerated()), id: \.offset) { index, step in
                    HStack {
                        Image(systemName: step.done ? "checkmark.square.fill" : "square")
                            .foregroundStyle(step.done ? Color.accentColor : .secondary)
                        Text(step.title).strikethrough(step.done).foregroundStyle(step.done ? .secondary : .primary)
                        Spacer()
                        if index == next {
                            Text("Next").font(.caption.bold()).foregroundStyle(Color.accentColor)
                        }
                    }
                    .font(.subheadline)
                }
            }
        }
        .padding()
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
