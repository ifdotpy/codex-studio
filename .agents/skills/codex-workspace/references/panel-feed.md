# Background data in the Studio panel

Use a panel feed for EC2 status, build counters, sensor readings, queue depth, or
other data that a script can read. The agent defines the display and starts the
script once. Later data flows from the script to the panel without model calls.
The process still uses local resources and can incur external API charges.

## Connect a script

1. Read the `orchestration_panel` catalog.
2. Publish a structured panel with `action=set` and initial `spec.state.live` data.
3. Bind display properties with paths such as `{"$state":"/live/count"}`.
4. Start the producer with `orchestration_panel_feed`:

```json
{
  "action": "start",
  "command": "python3 -u /absolute/path/to/status-feed.py",
  "statePath": "/live"
}
```

Write one complete JSON object per stdout line. Flush each line. For example:

```json
{"count":3,"status":"running","updatedAt":"2026-09-07T11:15:00Z"}
{"count":4,"status":"running","updatedAt":"2026-09-07T11:15:30Z"}
```

Each accepted line replaces the state subtree at `/live`. Include all fields that
the display reads from that subtree. A missing field is not a request to retain
its previous value. Keep local selection and form input outside the feed path,
for example `/selected`. Feed updates must not reset the user's selection.

Use `action=get` to inspect the current feed and `action=stop` to stop it.
These actions accept no other settings. Retain the returned monitor identity for
diagnostics. Each agent owns one panel feed. A new start replaces its previous
feed. Read the tool schema for limits and optional `timeout_ms`. Do not poll
feed state through model calls. Do not use a command monitor that sends each result
to the agent. The panel feed itself owns data delivery.

A producer supplies data only. It cannot change components, HTML, CSS, callbacks,
or another agent's panel. Data must still satisfy the component catalog and fit
inside the 150px viewport. Use the initial rendered image to review the layout.
Use compact states and local views for variable amounts of data.

Send diagnostics to stderr. On an unrecoverable read error, exit nonzero and keep
the previous successful data. Never publish zero, `complete`, or an empty result
as a substitute for a failed read. Include a source timestamp so old data remains
recognizable. A producer may implement bounded retries without calling the model.
Do not put credentials, tokens, or private command output in panel state.

## EC2 example

The maintained producer is `scripts/examples/ec2-panel-feed.py` in the Studio
application. Copy [the resource config](../assets/ec2-panel-resources.json) to a
project-owned runtime location. Its instance IDs and profile are examples.
Replace them with verified IDs, an authorized AWS profile, and the exact region.
The script requires explicit IDs. It never lists the entire AWS account.

Each resource has a short display `label`, `instanceId`, `profile`, and `region`.
The optional `resource` is the exact name in the project's resource board.
The example supports 1 to 12 instances. Use a separate feed composition for larger
sets instead of adding a scrolling list to the fixed-height panel.

Generate the initial panel from the real config:

```sh
python3 scripts/examples/ec2-panel-feed.py \
  --config /absolute/path/to/ec2-resources.json --panel-spec
```

Pass that JSON to `orchestration_panel`. The generated display uses Studio cloud
and server icons. A local instance selector lets each view fit inside the panel.
[The sample panel](../assets/ec2-panel.json) shows the same structure with sample IDs.
`Not checked` is the initial state; it does not claim a successful AWS read.

Check the read once before attaching it:

```sh
python3 scripts/examples/ec2-panel-feed.py \
  --config /absolute/path/to/ec2-resources.json \
  --claims-file /exact/project/board/codex-board.json --once
```

Then use the same command without `--once` as the feed command.
If the instance set changes, regenerate the panel and start a feed with the new
config. A feed does not discover or add unrelated instances. The default poll
interval is 30 seconds. `--interval` accepts 10 to 3600 seconds. Each AWS process
has a 15-second timeout by default; `--timeout` accepts 1 to 60 seconds.
`--panel-spec` makes no AWS request. `--once` emits one data object and exits.

Only `aws ec2 describe-instances` runs. The command groups explicit IDs by profile
and region. The profile's normal AWS credential chain remains outside the panel.
AWS response order is not used to match instances. An omitted requested instance
appears as `not returned`, rather than `stopped` or `terminated`.
See the [AWS command contract](https://docs.aws.amazon.com/cli/latest/reference/ec2/describe-instances.html).

## Distinguish machine state from work

EC2 `running` means that AWS reports the instance as running. It does not prove
that an agent uses it, a build runs, or CPU work occurs.

When the caller supplies `--claims-file`, the script reads that exact board on
each tick. A `claims` map entry uses `worker` or `thread` as its owner. The display
shows `Reserved` separately from the EC2 state. A reservation also does not prove
CPU work. The complete state includes the owner for another suitable composition.

When the board has no matching claim, the display says `No reservation`. When no
board or resource mapping was supplied, it says `Lease unknown`. An unreadable or
malformed supplied board stops the feed; it never becomes an empty claims map.

For Attar, use the board path supplied by its orchestrator or SDK cycle. Its
`docs/infra-aws.md` defines builder claims. Do not infer the current board from an
old path, an instance Name tag, or a previous session. Do not label every running
instance as involved in the current chat. The producer never claims, releases,
starts, stops, or terminates resources.
