# Property test recipe

For asserting invariants that hold over a wide input space. Uses `proptest` (already a dev-dependency in `reef`). Live under `tests/<name>_properties.rs`.

## When to use proptest vs. unit tests

Use proptest when:
- The property is a **universal quantifier**: "for all valid inputs, X holds"
- Example: "every row in `build_graph(commits)` has `cells[node_col] == Node`"
- Example: "`tree::build(files)` contains exactly as many `File` leaves as the input slice has entries"

Use unit tests when:
- The property is "for this specific input, X is exactly Y"
- Edge cases: empty input, zero, boundary values
- Regression tests for specific bugs

They're complementary. A typical module has both: a handful of unit tests pinning specific behaviors, plus proptest asserting invariants across random inputs.

## Basic skeleton

```rust
//! Property tests for <what>.

use proptest::prelude::*;

// Generate valid inputs — the hardest part of property testing.
fn any_valid_input() -> impl Strategy<Value = MyType> {
    // ... compose strategies
}

proptest! {
    #[test]
    fn property_name(input in any_valid_input()) {
        let result = my_function(&input);
        prop_assert!(holds(result, &input));
    }

    #[test]
    fn another_property(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        // proptest's own Strategy built-ins: any::<T>(), vec, option, etc.
    }
}
```

`prop_assert!` / `prop_assert_eq!` instead of `assert!` so proptest can shrink failing cases.

## Generating structured inputs

The strategy is the art. Bad strategies produce mostly-invalid inputs that fail their preconditions; the test becomes worthless.

### Example: topologically-ordered commit DAG

`build_graph` requires child-before-parent ordering. A naive `prop::collection::vec(any::<CommitInfo>(), ...)` would produce random commits with random parents — most would be invalid.

Pattern: generate a **shape** (number of commits + how many parents each has + offsets to pick parents from), then construct valid commits from the shape.

```rust
fn topo_commits(max_len: usize) -> impl Strategy<Value = Vec<CommitInfo>> {
    prop::collection::vec(
        (0usize..=2, 1usize..=100, 1usize..=100),  // (num_parents, off0, off1)
        1..=max_len,
    )
    .prop_map(|seeds| {
        let n = seeds.len();
        let mut commits = Vec::with_capacity(n);
        for (i, (num_parents, off0, off1)) in seeds.iter().enumerate() {
            let remaining = n.saturating_sub(i + 1);
            let mut parents = Vec::new();
            if remaining > 0 && *num_parents >= 1 {
                parents.push(format!("c{}", i + 1 + (off0 % remaining)));
            }
            if remaining > 0 && *num_parents >= 2 {
                parents.push(format!("c{}", i + 1 + (off1 % remaining)));
            }
            parents.sort(); parents.dedup();
            commits.push(CommitInfo { oid: format!("c{}", i), parents, /* ... */ });
        }
        commits
    })
}
```

Key moves:
1. Generate a **Vec of simple tuples** (proptest shrinks these well)
2. Map the shape to valid structures
3. Rely on `%` to keep parent indices in range (instead of filtering, which slows generation)

See `tests/git_graph_properties.rs` for the complete version.

## Useful property patterns

### Roundtrip

```rust
prop_assert_eq!(decode(encode(&msg)).unwrap(), msg);
```

Asserts `decode ∘ encode = id`. Catches asymmetric bugs where encode produces something decode can't read back.

### Never panic

```rust
proptest! {
    #[test]
    fn parser_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = my_parser(&bytes);  // may be Err — must not unwind
    }
}
```

Use for any deserializer / parser that consumes untrusted input. The `build_graph` fuzz target in `fuzz/fuzz_targets/build_graph.rs` uses the same never-panic pattern outside proptest.

### Structural invariants

```rust
for row in build_graph(&commits) {
    prop_assert!(row.node_col < row.cells.len());
    prop_assert_eq!(row.cells[row.node_col], LaneCell::Node);
    for cell in &row.cells {
        match cell {
            LaneCell::Fork { to } => prop_assert!(*to < row.cells.len()),
            LaneCell::Merge { from } => prop_assert!(*from < row.cells.len()),
            _ => {}
        }
    }
}
```

When an algorithm has non-trivial invariants that hold by construction, assert them. Catches refactors that accidentally violate them.

### Length / count relationships

```rust
prop_assert_eq!(build_graph(&commits).len(), commits.len());
```

Simple but catches whole classes of bugs — missing/extra iterations, early returns, accidental dedup.

## When a property fails

Proptest prints the **shrunk** input — the smallest failing case it could find. Read it carefully:

```
Test failed: property 'node_col_in_bounds' failed
Input: commits = [
    CommitInfo { oid: "c0", parents: [] },
    CommitInfo { oid: "c1", parents: ["c0"] },    // ← likely the issue
]
```

The shrunk case is usually the minimal reproducer you'd want in a unit test. After fixing, **add it as a unit test** so the same regression can't come back.

## Don't

- Don't make property assertions on floating-point equality without a tolerance
- Don't use proptest for anything whose expected output you can't express algorithmically — property tests need a runtime property, not a lookup table
- Don't run proptest with `proptest_cases = 1_000_000` — the default (256 cases) is fine; CI will catch flakiness faster with a reasonable count
