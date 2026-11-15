# Dynamic workspaces and overview

Each output has its own ordered workspace list. Workspace indices are zero based
in IPC and one based on keyboard shortcuts. Occupying the trailing empty workspace
appends another empty workspace. Empty inactive workspaces are removed after a
switch or gesture settles; workspace IDs survive index changes. Window geometry,
mode state and focus order stay attached to their workspace while hidden.

Mod+1 through Mod+9 select an existing workspace, including the trailing empty
workspace. Mod+Shift+number moves the focused window there. Mod+Tab and
Mod+Shift+Tab cycle; Mod+Ctrl+Page Down/Up select next/previous. Mod+A, an
unaccompanied Super release, and Shift+Super+F23 toggle overview. Esc closes it.

In overview, select a window to focus it and return to the desktop. Select a
thumbnail to change workspace, drag a window onto a thumbnail to move it, and
middle click a window to request that it close. Three-finger tap uses libinput's
standard middle-click mapping. Typing is ignored; launcher search is deferred.

Three-finger horizontal swipes drive the workspace strip directly; vertical
swipes drive overview progress. The dominant axis locks after 12 input units.
Release projects velocity by 0.18 seconds and selects the nearest workspace or
open/closed overview state. Three-finger hold catches the current animation;
release settles to the nearest state. Two-finger gestures are forwarded through
pointer-gestures unless Super was held at begin. Three/four-finger pinches and
four-finger swipes are reserved for later desktop/window actions.

`GAVNO_GESTURE_DISTANCE` controls logical input units per workspace (default 300,
minimum 24). Overview uses the mockup's 340-unit vertical distance.
`GAVNO_NATURAL_SCROLL=0` reverses gesture direction and disables natural scrolling
on touchpads; natural scrolling is enabled by default.

IPC additions, using the existing newline-delimited JSON envelope:

- `workspace`, `{}`: query active output workspace/count and stable IDs.
- `workspace`, `{"output":"eDP-1","workspace":1}`: select a workspace.
- `move_to_workspace`, `{"id":7,"workspace":1}`: move a window on its output.
- `screen`: includes all owned windows, their `workspace`, and each output's
  current `workspace` and `workspaces` count. Hidden windows retain real geometry.

Selection/movement reject indices beyond the trailing empty workspace. Inactive
windows stay outside Space, receive no input or frame callbacks, and only appear
as read-only live content in overview thumbnails or the switching strip. No
workspace or overview timers run while idle.
