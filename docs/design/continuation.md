# Continuation

Continuation is on by default. When a WebSocket request extends the transcript
of a recorded response, duolane sends `previous_response_id` with only the new
items, so it sends less input at every step. `codex.previousResponseId` is a
JSON boolean, and `DUOLANE_CODEX_PREVIOUS_RESPONSE_ID` overrides it with `1`,
`true`, `yes`, or `on`, and `0`, `false`, `no`, or `off`. Any other value falls
through to the configuration.

## Principle

With `previous_response_id` the backend continues from the response it stored,
not from the history the client sends. The model then reads the backend's copy
of every earlier step. So continuation is allowed only when the backend's copy
and the client's history are known to say the same thing, and every doubt sends
the full context.

duolane sometimes changes what the model produced before the client sees it: it
rewrites some tool arguments, drops item kinds it cannot carry, and joins
streamed text. Each of those makes the two copies differ, so each must be
detected.

## Recording a response

When a response completes, duolane compares two lists:

- **Stored**: the output items that the final upstream event lists for the
  response. This is the backend's copy.
- **Echoed**: the blocks duolane actually sent to the client for that response,
  kept as they went out and translated back to input items by the same code that
  translates the client's next request. duolane keeps these blocks up to the
  [buffered response limit](security.md#limits); a larger response is not
  recorded.

duolane records the response as something to continue from only when the lists
have the same length and each pair of items is equal under these rules, and only
these:

| Stored item   | Recorded only when                                                                                                  |
| ------------- | ------------------------------------------------------------------------------------------------------------------- |
| Message       | It is from the assistant, every part is output text without annotations, and the echoed text is the same            |
| Reasoning     | The echoed item has the same id and the same encrypted content                                                      |
| Function call | It has no namespace, and the echoed call has the same call id, the same name, and the same arguments as JSON values |

Every other stored item records nothing: a custom tool call, a hosted tool call,
a refusal, a message with a citation, a function call in a namespace, and any
kind the table does not name. So do a final event without an output list, an
item on one side only, and any difference under the table. A rewritten tool
argument, a dropped item, and text that differs between the stream and the final
event are caught this way, without a rule for each.

Two fields are not compared: the phase of a message and the summary of a
reasoning item. The client's history carries no phase, and the translation of a
signed thinking block restores the reasoning item with its id, its encrypted
content, and an empty summary. The backend keeps both fields in its own copy, so
continuing keeps them for the model, and sending the full context loses them, as
it does today. A thinking block without a valid signature becomes text, which
does not match.

A response is also not recorded when it did not end with a completed terminal
event on a WebSocket whose socket returned to the pool.

## Continuing

A request sends only its new items when all of these hold at the moment it is
sent, and its full context otherwise:

- The conversation has a recorded response that is at most 30 minutes old, on
  the socket that received it.
- The request is not a [compaction request](codex.md#compaction-requests), and
  server compaction is off.
- Every field of the translated request other than its input equals the previous
  request's, and the account is the same.
- The translated input begins with the recorded transcript and adds at least one
  item. Each item is compared whole, every field of it, so a changed image,
  instruction, tool list, or tool result does not match. The one exception is
  `function_call.arguments`, compared as JSON values, because the recorded
  transcript keeps the text the model streamed while the next request
  re-serializes the client's parsed input.

The comparison is on the translated request. A change sends the full context
when it changes a request field or breaks the translated prefix, as a changed
system prompt, a changed tool list, and a compacted history do. An edit that
translates to the same items, such as changed thinking text with its signature
kept, still continues.

- Stale continuation state is dropped before a retry picks another transport or
  connection.
- When the backend no longer has the previous response, it refuses the request.
  duolane then sends the full context at most once, under the replay rules of
  [transport and retries](codex.md#transport-and-retries); after output or a
  hosted tool ran, the request fails instead.

## Limits of the proof

The stored list is what the backend reports, not what it keeps. No source says
the two are always the same on the ChatGPT backend, so
[unverified behavior](codex.md#unverified-behavior) lists it.
