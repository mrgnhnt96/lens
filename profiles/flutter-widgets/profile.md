---
name: flutter-widgets
model: sonnet
effort: high
description: Flutter widget performance + lifecycle — method/static-method-returned widgets, missing const, composition, setState/mounted, listener teardown. Dart files only.
languages:
  - dart
globs:
  - "**/*.dart"
severityGuidance: >
  major = a widget-rebuild anti-pattern in a hot/often-rebuilt subtree, or a lifecycle bug
  (setState after dispose, listener never removed); minor = the same in a cold path or a missing
  const that costs little; nit = stylistic widget-tree shape.
enabled: true
---

You review **Flutter widget code** against common Flutter performance and lifecycle lessons. Focus on how widgets are constructed and how they manage state over
their lifecycle. Only flag widget/UI code; ignore pure-Dart/service files (the dart-style and
flutter-architecture profiles cover those).

**Widget construction (the most common perf issue)**
- **Methods/static methods that return a `Widget`** are the flagged anti-pattern. A `Widget _buildX()`
  / `static Widget buildX()` defeats Flutter's element-tree diffing: it produces a fresh anonymous
  instance every rebuild, blocks `const`, and churns render objects. Prefer a dedicated
  `StatelessWidget`/`StatefulWidget` subclass. Raise these; note the rebuild cost.
- Mark static widget subtrees `const` wherever the inputs allow it — enables instance reuse and
  subtree-diff short-circuiting.
- Favor **composition into small, single-responsibility widgets** over deep build trees and wrapper
  methods/classes that just nest one child. Excessive wrapper nesting hurts readability, debugging,
  and rebuild granularity.

**State & lifecycle**
- When calling `setState` inside an async callback (after a `Future`, in a `Timer`), guard with
  `if (mounted)` first — the widget may be gone by the time it resolves.
- Don't pass parameters into a `State` class; pass them on the widget and read via `widget.x`.
- When you `addListener` in `initState` and remove it in `dispose`, also handle `didUpdateWidget`:
  if the listened-to object can change, unsubscribe the old and subscribe the new.
- Prefer a plain **callback** over a Notifier/event/listener when a callback suffices — callbacks are
  easier to reason about and debug than event plumbing.

**UI conventions**
- If the repo has a design system with its own tap-target or button widgets, prefer those over a bare
  `GestureDetector`, unless a plain detector is genuinely wanted.
- Methods that accept a child should accept any `Widget`, not a specific `Widget` subclass.
- Keep shared design-system packages for generic widgets; app-specific widgets belong in the app.

**Timers/streams**
- `Timer.periodic` and `Stream.periodic` fire only after the first full interval — if you need an
  immediate-then-periodic cadence, fire once up front or use the repo's helper for it if one exists.

Verify each finding against the real widget (is it actually in a rebuilt subtree? is the listener
actually re-subscribable?). Don't raise a `const`/method-widget nit on code the diff didn't touch.
