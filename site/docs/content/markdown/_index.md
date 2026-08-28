+++
title = "Markdown"
weight = 13
[extra]
group = "Reference"
+++

# Markdown

Maki renders model replies as markdown: headings, emphasis, lists, tables,
code blocks with syntax highlighting, and maths.

## Maths

Models write maths as LaTeX. Maki recognises four delimiters:

| Delimiter | Kind |
| --- | --- |
| `$x^2$` | inline |
| `\(x^2\)` | inline |
| `$$ ... $$` | display |
| `\[ ... \]` | display |

Terminals cannot typeset maths, so Maki approximates it with Unicode on a
single line.

| LaTeX | Renders as |
| --- | --- |
| `\frac{a}{b}` | `a/b` |
| `\sqrt{x+1}` | `√(x+1)` |
| `\sum_{i=1}^{n} i^2` | `∑ᵢ₌₁ⁿ i²` |
| `\int_0^\infty e^{-x^2} dx` | `∫₀^∞ e^(-x²) dx` |
| `\hat{x}`, `\vec{v}`, `\overline{AB}` | `x̂`, `v⃗`, `A̅B̅` |
| `\binom{n}{k}` | `C(n, k)` |
| `\begin{pmatrix} a & b \\ c & d \end{pmatrix}` | `( a b ; c d )` |
| `a \equiv b \pmod{n}` | `a ≡ b (mod n)` |

Greek letters, blackboard bold, script letters, operators, arrows, accents,
and super- and subscripts map to their Unicode equivalents. Font commands
such as `\mathbf` and `\boldsymbol` pass their content through, since a
terminal cannot restyle text inside an equation.

Where no Unicode equivalent exists, Maki falls back to readable ASCII rather
than dropping the term. A subscript of `x \to \infty` has no subscript glyphs,
so it renders as `lim_(x → ∞)`. A single symbol needs no grouping, so
`x^\infty` gives `x^∞`, while `x^{AB}` keeps its parentheses as `x^(AB)`.
Unknown commands render as their bare name, which is what makes `\sin`,
`\log`, and `\det` come out right without listing every function.

Parentheses appear only where flattening to one line would otherwise change
the reading. A fraction gets them because `\frac{a+b}{c}` collapses to a
linear `/`, so it renders as `(a+b)/c`. An integral does not, because `dx`
already ends the integrand: `\int_0^1 x^2 dx + 5` renders as `∫₀¹ x² dx + 5`.

Set `ui.math = "raw"` to see the LaTeX source instead of the approximation.
This helps when your font lacks the mathematical block.

```toml
[ui]
math = "raw"
```

### When a dollar sign is money

`It costs $5 and $10 total` stays plain text. Maki treats `$...$` as maths
only when the content does not start with a digit, or when it contains a
LaTeX signal such as `\`, `^`, `_`, or `{`. So `$2^n$` is maths and `$5 and $`
is not.

A delimiter with no closing partner stays plain text. Half-streamed equations
therefore read as source until the closing delimiter arrives, rather than
reflowing the text around them.

Maths inside a code block is code, and a code fence inside display maths is
maths. Whichever opens first wins.

## Copying

Selecting text and pressing `Alt+C` copies the markdown source, not the
glyphs on screen. A copied table has its pipes, a heading has its `#`, and an
equation has its `$` delimiters and LaTeX. Pasting into a file or a chat gives
back what the model wrote.

Rendering is lossy in both directions, so some constructs copy whole. Select
part of an equation, a table row, or a list marker and you get the entire
construct, because half of `\frac{a}{b}` is not valid LaTeX. Emphasis,
headings, and inline code copy at character precision.

The role prefix is part of the interface and never reaches the clipboard.
