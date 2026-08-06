#import "../lib.typ": *
#import "../peroxide.typ": solve-ode

#set page(width: 150mm, height: auto, margin: 18mm)
#set text(size: 10.5pt)
#set par(leading: 0.7em)

= Symbolic equations, numerical trajectory

This Lotka–Volterra model crosses the plugin boundary as Symbolica atoms. Its
coefficients remain exact rationals while Tymbolica constructs the model;
the separate Peroxide plugin performs only the numerical time integration.

#let t = var("t")
#let x = var("x")
#let y = var("y")
#let rhs = (
  math($2/3 x - 4/3 x y$),
  math($x y - y$),
)

$
  dif x / dif t &= #to-typst(rhs.at(0)), \
  dif y / dif t &= #to-typst(rhs.at(1)).
$

The parameter order is time first, followed by the two state variables.

#let model = atom-model(rhs, (t, x, y))
#let trajectory = solve-ode(
  model,
  (0, 6),
  0.125,
  (1, 1),
)

#assert.eq(trajectory.first(), (0.0, 1.0, 1.0))

Selected rows from the 48 fixed steps are shown below.

#let selected = (0, 12, 24, 36, 48).map(index => trajectory.at(index))
#let rounded(value) = str(calc.round(value, digits: 4))

#table(
  columns: (1fr, 1fr, 1fr),
  align: (center, center, center),
  inset: 6pt,
  stroke: 0.5pt + luma(75%),
  table.header([$t$], [$x(t)$], [$y(t)$]),
  ..selected.map(row => (
    [#rounded(row.at(0))],
    [#rounded(row.at(1))],
    [#rounded(row.at(2))],
  )).flatten(),
)
