# Agent Loop

## Goal

The loop should be simple enough to reason about and rich enough to recover from mistakes.

```text
receive request
  -> classify task locally
  -> assemble context plan
  -> model inference
  -> if tool calls: authorize + execute
  -> normalize/trim tool results
  -> append result
  -> repeat
  -> verification gate
  -> final response
```

## Do not add mandatory planner calls

A separate planner model call for every request increases latency and tokens. Use deterministic heuristics first:

- trivial question / explanation: answer directly
- one-file obvious edit: inspect + edit + validate
- multi-module feature or migration: enable explicit plan state
- ambiguous destructive request: permission/clarification gate

The primary model can update task state through a compact plan tool when necessary.

## Task state

Store machine-readable state outside the prompt:

```text
objective
constraints
workspace
relevant_files
relevant_symbols
assumptions
planned_steps
completed_steps
changes
verification_results
open_failures
```

Only the subset useful to the next model call is rendered.

## Tool-call batching

Independent reads/searches may execute concurrently. Dependent commands remain sequential. Do not parallelize write operations into the same working tree unless they use isolated worktrees.

## Failure recovery

### Patch failure
Re-read the smallest relevant range, regenerate a new patch, retry with a bounded count.

### Test failure
Parse the failing test names and stack/error regions locally. Return the minimal evidence to the model, not the entire build log.

### Command timeout
Kill the process group, preserve the tail of output, classify timeout vs deadlock vs waiting-for-input.

### Tool schema failure
Do not “guess” corrected arguments silently. Validate locally, return structured validation errors.

## Verification gate

Before the final answer after source edits:

1. format touched files if the repository standard requires it;
2. compiler/type check for impacted module;
3. targeted tests;
4. broader tests if change radius justifies them;
5. inspect git diff;
6. scan diff for secret leakage or accidental generated files.

If validation cannot run, report the exact reason and what remains unverified.

## Stop conditions

- final model message after verification
- user interrupt
- permission denied and no safe alternative
- iteration budget exhausted
- provider context limit cannot be recovered via compaction
- repeated identical failure signature beyond retry limit

Avoid infinite loops by hashing recent `(tool, args, error)` tuples and detecting repetition.
