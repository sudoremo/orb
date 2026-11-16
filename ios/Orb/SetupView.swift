import OrbKit
import PhotosUI
import SwiftUI

/// The first launch: three promises, then the account the tasks live on.
struct SetupView: View {
    @Environment(AppModel.self) private var model

    @State private var server = ""
    @State private var port = "993"
    @State private var username = ""
    @State private var password = ""
    @State private var folder = "Tasks"
    @State private var working = false
    @State private var problem: String?
    /// A setup whose folder the server does not have yet, waiting for a yes.
    @State private var missingFolder: Pending?
    @State private var scanning = false
    @State private var photo: PhotosPickerItem?

    /// What is set up once the account check has passed.
    struct Pending {
        let setup: TasksAccountSetup
        let password: String
        let today: TodaySettings?
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    promise("server", "Your server, nobody else's", "Orb talks to your IMAP server directly. No account with us, no cloud in between.")
                    promise("icloud.dashed", "Works offline", "Changes wait on the iPhone and upload by themselves.")
                }
                Section {
                    if CodeScanner.isAvailable {
                        Button("Scan the Code on Your Mac", systemImage: "qrcode.viewfinder") { scanning = true }
                    }
                    PhotosPicker(selection: $photo, matching: .images) {
                        Label("Choose a Photo of the Code", systemImage: "photo")
                    }
                } footer: {
                    Text("On the Mac: Tasks ▸ Set Up iPhone…. The code sets up everything, the password included.")
                }
                Section("Or Set Up Manually · Server") {
                    TextField("imap.example.com", text: $server)
                        .textContentType(.URL)
                        .keyboardType(.URL)
                    TextField("Port", text: $port)
                        .keyboardType(.numberPad)
                }
                Section("Account") {
                    TextField("Username", text: $username)
                        .textContentType(.username)
                        .keyboardType(.emailAddress)
                    SecureField("Password", text: $password)
                        .textContentType(.password)
                }
                Section {
                    TextField("Folder", text: $folder)
                } header: {
                    Text("Tasks Folder")
                } footer: {
                    Text("The folder Orb on the Mac keeps the tasks in, spelled exactly the same.")
                }
            }
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .disabled(working)
            .alert(
                "Could Not Set Up Orb",
                isPresented: Binding(get: { problem != nil }, set: { if !$0 { problem = nil } })
            ) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(problem ?? "")
            }
            .overlay { if working { ProgressView("Checking the Account…") } }
            .sheet(isPresented: $scanning) { CodeScannerSheet { use(code: $0) } }
            .onChange(of: photo) { _, item in
                guard let item else { return }
                photo = nil
                read(item)
            }
            .confirmationDialog(
                "There is no folder “\(missingFolder?.setup.folder ?? "")” on the server yet.",
                isPresented: Binding(get: { missingFolder != nil }, set: { if !$0 { missingFolder = nil } }),
                titleVisibility: .visible
            ) {
                Button("Create It") {
                    if let pending = missingFolder { finish(pending) }
                }
                Button("Change the Name", role: .cancel) {}
            } message: {
                Text("Orb creates it on its first sync. If Orb on your Mac keeps its tasks under another name, use that name instead.")
            }
            .navigationTitle("Set Up Orb")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Continue", systemImage: "checkmark", role: .confirm, action: submit)
                        .disabled(!complete || working)
                }
            }
        }
    }

    private var complete: Bool {
        !server.isEmpty && UInt16(port) != nil && !username.isEmpty && !password.isEmpty && !folder.isEmpty
    }

    private func promise(_ symbol: String, _ title: String, _ text: String) -> some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).bold()
                Text(text).font(.subheadline).foregroundStyle(.secondary)
            }
        } icon: {
            Image(systemName: symbol).foregroundStyle(Color.accentColor)
        }
    }

    private func submit() {
        guard let port = UInt16(port) else { return }
        let setup = TasksAccountSetup(
            imapServer: server.trimmingCharacters(in: .whitespaces),
            imapPort: port,
            imapUsername: username.trimmingCharacters(in: .whitespaces),
            folder: folder
        )
        check(Pending(setup: setup, password: password, today: nil))
    }

    /// A scanned or photographed code: everything at once, password included.
    func use(code: String) {
        do {
            let code = try readSetupCode(text: code)
            check(Pending(setup: code.setup, password: code.password, today: code.today))
        } catch {
            problem = error.message
        }
    }

    private func read(_ item: PhotosPickerItem) {
        Task {
            guard let data = try? await item.loadTransferable(type: Data.self),
                  let image = UIImage(data: data)?.cgImage,
                  let text = SetupCodeImage.payload(in: image)
            else {
                problem = "There is no Orb setup code in this photo."
                return
            }
            use(code: text)
        }
    }

    /// Log in and look for the folder before anything is saved.
    private func check(_ pending: Pending) {
        working = true
        problem = nil
        let (setup, password) = (pending.setup, pending.password)
        Task {
            do {
                let check = try await Task.detached { try checkTasksAccount(setup: setup, password: password) }.value
                working = false
                if check.folder == nil {
                    missingFolder = pending
                } else {
                    finish(pending)
                }
            } catch {
                problem = error.message
                working = false
            }
        }
    }

    private func finish(_ pending: Pending) {
        working = true
        Task {
            do {
                try await model.setUp(pending.setup, password: pending.password, today: pending.today)
            } catch {
                problem = error.message
            }
            working = false
        }
    }
}
