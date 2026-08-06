// A deliberately small numerical companion to Tymbolica. Expressions are
// created by the main plugin and crossed into this plugin as an atom model.
#let _plugin = plugin("tymbolica-peroxide.wasm")

#let _number(value, label) = {
  assert(
    type(value) in (int, float),
    message: label + " must be an integer or float",
  )
  float(value)
}

/// Solve an initial-value problem with Peroxide's fixed-step RK4 integrator.
///
/// `model` must come from Tymbolica's `atom-model`. Its parameters are ordered
/// as time followed by the state variables, and `initial` follows that state
/// order. Do not substitute externally supplied bytes. The returned rows have
/// the form `(t, y_0, y_1, ...)`. Because this example deliberately uses
/// fixed-step RK4, `step-size` must evenly divide `t-span`.
#let solve-ode(model, t-span, step-size, initial) = {
  assert.eq(type(model), bytes, message: "model must be bytes returned by atom-model")
  assert(model.len() > 0, message: "model must not be empty")
  assert(
    type(t-span) == array and t-span.len() == 2,
    message: "t-span must be a pair (start, end)",
  )
  assert.eq(type(initial), array, message: "initial must be an array")
  assert(initial.len() > 0, message: "initial must contain at least one state value")

  let start = _number(t-span.at(0), "t-span start")
  let end = _number(t-span.at(1), "t-span end")
  let step = _number(step-size, "step-size")
  let initial = initial.enumerate().map(pair => {
    _number(pair.at(1), "initial[" + str(pair.at(0)) + "]")
  })

  assert(start < end, message: "t-span start must be less than its end")
  assert(step > 0.0, message: "step-size must be positive")
  assert(step <= end - start, message: "step-size must not exceed the time span")

  let config = (
    t-span: (start, end),
    step-size: step,
    initial: initial,
  )
  cbor(_plugin.solve_rk4(model, cbor.encode(config)))
}
