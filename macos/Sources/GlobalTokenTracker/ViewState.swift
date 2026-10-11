// `@ViewState` — a plain property wrapper over SwiftUI's `State`.
//
// In the macOS 26+ SDKs `@State` is a compiler macro whose plugin ships only
// with Xcode, so a Command Line Tools–only toolchain can't expand it. The
// `State` struct itself is unchanged, and SwiftUI installs nested dynamic
// properties recursively, so wrapping it keeps identical storage, lifetime
// and invalidation semantics while building with `swift build` alone.
import SwiftUI

@propertyWrapper
struct ViewState<Value>: DynamicProperty {
    private let state: State<Value>

    init(wrappedValue: Value) {
        state = State(initialValue: wrappedValue)
    }

    var wrappedValue: Value {
        get { state.wrappedValue }
        nonmutating set { state.wrappedValue = newValue }
    }

    var projectedValue: Binding<Value> { state.projectedValue }
}
