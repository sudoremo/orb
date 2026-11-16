import OrbKit
import SwiftUI

/// Renaming, recolouring and deleting tags (§7), the same wherever tags are
/// listed: swipe actions on each row, and the prompts they open.
@MainActor @Observable
final class TagEditor {
    var renaming: TaskTagInfo?
    var newName = ""
    var deleting: TaskTagInfo?

    func rename(_ tag: TaskTagInfo, in model: AppModel) {
        let name = newName.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, name != tag.name else { return }
        let (id, colour) = (tag.id, tag.colour)
        Task { await model.perform("Renamed #\(tag.name)") { _ = try $0.updateTag(tagId: id, name: name, colour: colour) } }
    }

    func recolour(_ tag: TaskTagInfo, _ colour: String?, in model: AppModel) {
        let (id, name) = (tag.id, tag.name)
        Task { await model.perform { _ = try $0.updateTag(tagId: id, name: name, colour: colour) } }
    }

    func delete(_ tag: TaskTagInfo, in model: AppModel) {
        let id = tag.id
        Task { await model.perform("Deleted #\(tag.name)") { _ = try $0.deleteTag(tagId: id) } }
    }
}

/// A tag row's swipe actions: delete, rename, colour.
struct TagSwipeActions: View {
    @Environment(AppModel.self) private var model
    let tag: TaskTagInfo
    let editor: TagEditor

    var body: some View {
        Button("Delete", systemImage: "trash", role: .destructive) { editor.deleting = tag }
        Button("Rename", systemImage: "pencil") {
            editor.newName = tag.name
            editor.renaming = tag
        }
        .tint(.indigo)
        Menu("Colour", systemImage: "paintpalette") {
            ForEach(TagColour.allCases) { colour in
                Button(colour.name) { editor.recolour(tag, colour.hex, in: model) }
            }
            Button("No Colour") { editor.recolour(tag, nil, in: model) }
        }
        .tint(.orange)
    }
}

extension View {
    /// The rename prompt and the delete confirmation of `editor`.
    func tagEditing(_ editor: TagEditor, model: AppModel) -> some View {
        modifier(TagEditingPrompts(editor: editor, model: model))
    }
}

private struct TagEditingPrompts: ViewModifier {
    @Bindable var editor: TagEditor
    let model: AppModel

    func body(content: Content) -> some View {
        content
            .alert("Rename Tag", isPresented: Binding(get: { editor.renaming != nil }, set: { if !$0 { editor.renaming = nil } })) {
                TextField("Name", text: $editor.newName)
                Button("Rename") {
                    if let tag = editor.renaming { editor.rename(tag, in: model) }
                }
                Button("Cancel", role: .cancel) {}
            }
            .confirmationDialog(
                "Delete the tag “\(editor.deleting?.name ?? "")”?",
                isPresented: Binding(get: { editor.deleting != nil }, set: { if !$0 { editor.deleting = nil } }),
                titleVisibility: .visible
            ) {
                Button("Delete Tag", role: .destructive) {
                    if let tag = editor.deleting { editor.delete(tag, in: model) }
                }
            } message: {
                Text("The tag comes off its tasks. No task is deleted.")
            }
    }
}
