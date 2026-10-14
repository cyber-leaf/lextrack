# How the M8 handles buttons (from the firmware dump)

Notes from reading a Ghidra decompile of the M8 firmware (`decompiled.c`,
1,741 functions, attached in the project thread). They describe behaviour
only; no firmware code, tables or assets are copied into lextrack.

## What the dump does and doesn't show

- It has no strings and no data section. Every on-screen label, the font,
  the theme, the view objects' method tables and the effect name list sit in
  data the dump only points at (`DAT_...`). Screen layouts and labels can't
  be read from it, so lextrack's screens stay as they are.
- What it does show is the button logic. Each view has a handler that gets
  one byte with the buttons held down, and the bits match the M8's headless
  protocol: LEFT 0x80, UP 0x40, DOWN 0x20, SHIFT 0x10, PLAY 0x08,
  RIGHT 0x04, OPTION 0x02, EDIT 0x01. A value of 0 means everything was let
  go. The handlers read are song, chain, phrase, instrument, table, the
  instrument list and the file browser.
- The firmware is newer than the file format lextrack reads (m8-js, up to
  2.7). Its effect numbers differ: GRV 03, SCA 10, TBL 14, TBX 17. lextrack
  keeps the 2.7 numbering and looks effects up by name.

## Views

The view switcher takes a number from 0x00 to 0x1D. Those confirmed by the
handlers that jump to them:

| No. | View | How you get there |
|-----|------|-------------------|
| 00 | Song | SHIFT+LEFT from chain |
| 01 | Groove | SHIFT+UP from phrase |
| 02 | Chain | SHIFT+RIGHT from song, SHIFT+LEFT from phrase |
| 03 | Phrase | SHIFT+RIGHT from chain, SHIFT+LEFT from instrument |
| 04 | Table | SHIFT+RIGHT from instrument, or from a TBL/TBX effect |
| 05 | Instrument | SHIFT+RIGHT from phrase |
| 06 | Project | SHIFT+UP from song and chain |
| 09 | Effect list | EDIT+UP/DOWN on an effect command |
| 0D | Shared bottom view (probably the mixer) | SHIFT+DOWN from song, chain, phrase, instrument and table |
| 13 | Scale | SHIFT+UP from phrase with the cursor on an SCA effect |
| 16 | Shared top view above instrument and table | SHIFT+UP from instrument and table |
| 19 | EQ | SHIFT+RIGHT from an instrument with an EQ set, or from an EQ effect |

The mini-map column lit for each view is song 0, chain 1, groove, phrase and
scale 2, instrument 3, table 4.

## Buttons in every grid view (song, chain, phrase, table)

- Arrows move the cursor. In the song, OPTION+UP/DOWN goes through the same
  cursor code as plain arrows. lextrack's page jump is kept because the
  modifier handling inside that code isn't visible.
- EDIT on its own puts in the last value. EDIT+LEFT/RIGHT is -1/+1.
  EDIT+DOWN/UP is -16/+16, or -12/+12 on a note, a chain's transpose and a
  table's transpose column.
- EDIT+UP/DOWN on an effect command opens the effect list instead of
  stepping. EDIT+LEFT/RIGHT still steps through the commands.
- EDIT+OPTION deletes. SHIFT+OPTION starts a selection, and pressing it again
  selects the column, then whole rows, then everything, then stops.
  SHIFT+EDIT pastes.
- PLAY and SHIFT+PLAY go to the view's play action. In song and chain,
  LEFT+PLAY works like PLAY.

## Editing a selection

With a selection held, EDIT+arrows change every selected cell at once:

- Each value moves by the step on its own (relative, not set to one value):
  ±1 for LEFT/RIGHT, and ±16 (±12 on notes and transposes) for UP/DOWN.
  Values stop at their limits.
- Empty cells stay empty, and note offs are skipped.
- In the song, EDIT+UP/DOWN moves the selected rows up or down by one row.
  EDIT+LEFT/RIGHT works when one column is selected, and empty slots in it
  get the column's first chain plus the step.
- The firmware hands phrase and table selections that span several columns
  to a function that isn't in the dump. lextrack applies the same per-cell
  rule to each column, which is an inference.
- When the row next to a moving song block isn't empty, the firmware runs
  extra logic that can't be followed with confidence. lextrack doesn't move
  the block then and shows "NO ROOM TO MOVE".

## Song

- SHIFT+RIGHT opens the chain under the cursor. On an empty slot it opens the
  nearest chain above it in the same track, or chain 00.
- SHIFT+UP goes to project and SHIFT+DOWN goes to view 0D. SHIFT+LEFT turns
  live mode on or off.
- When a selection is held, EDIT+UP/DOWN moves the selected rows up or down.

## Chain

- SHIFT+RIGHT opens the phrase on the cursor row. On an empty step it uses
  the nearest phrase above it and moves the cursor there. If the chain has
  none, the cursor goes to row 0 and a message shows.
- OPTION+LEFT/RIGHT goes to the next track that has a chain on this song row,
  skipping empty tracks. OPTION+UP/DOWN goes one song row up or down, and
  stays put when that row has no chain in this track.
- Changing a transpose shows its value in the message line.

## Phrase

- SHIFT+RIGHT opens the instrument used on or above the cursor row. With the
  cursor on a TBL/TBX effect it opens that table instead (and an EQ or
  instrument effect opens that EQ or instrument).
- SHIFT+UP opens the groove set by the GRV effect on or above the cursor row,
  or the scale when the cursor is on an SCA effect.
- OPTION+UP/DOWN goes to the previous/next phrase in the chain.
  OPTION+LEFT/RIGHT goes to the next track that has a phrase.
- EDIT+DOWN on the instrument column can open view 18. That is probably the
  instrument list, and SHIFT+LEFT from the instrument goes back to it.

## Instrument

- OPTION+LEFT/RIGHT is the instrument number -1/+1. **OPTION+UP adds 16 and
  OPTION+DOWN takes 16 off.**
- EDIT+arrows edit values, and they work with PLAY held too.
- SHIFT+LEFT goes back to phrase. SHIFT+RIGHT opens the table, or the
  instrument's EQ when it has one. SHIFT+UP goes to view 16 and SHIFT+DOWN
  goes to view 0D.
- EDIT+PLAY and OPTION+PLAY have their own actions here.
- LEFT+UP+DOWN+RIGHT+EDIT held together randomises the instrument's
  settings.

## Table

- OPTION+LEFT/RIGHT is the table number -1/+1, and **OPTION+UP adds 16,
  OPTION+DOWN takes 16 off**. For tables 00-7F the current instrument
  changes to match.
- SHIFT+LEFT goes back to the view you came from, or to the instrument with
  the same number.
- SHIFT+RIGHT on a TBL/TBX effect opens that table.

## Where lextrack differed, and what changed

| Behaviour | Before | Now |
|-----------|--------|-----|
| OPTION+UP/DOWN on instrument and table | UP was -16 | UP is +16, DOWN is -16 |
| EDIT+UP/DOWN on transpose (chain and table) | ±16 | ±12 |
| EDIT+UP/DOWN on an effect command | stepped the command | opens an effect list. Arrows move, EDIT (or letting go of EDIT) picks, OPTION cancels |
| SHIFT+RIGHT on an empty song slot | "NO CHAIN HERE" | opens the chain above, or 00 |
| SHIFT+RIGHT on an empty chain step | "NO PHRASE HERE" | opens the phrase above and moves the cursor there |
| SHIFT+RIGHT on a TBL effect (phrase or table) | opened the instrument | opens that table, and SHIFT+LEFT comes back |
| SHIFT+UP from phrase | the last groove viewed | the groove of the GRV on or above the row |
| OPTION+LEFT/RIGHT in chain and phrase | stopped at an empty track | skips to the next track with something on it |
| Table number changes | instrument stayed | the instrument follows for 00-7F |
| EDIT+arrows on a selection | did nothing | moves every selected value by the step. In the song, UP/DOWN moves the selected rows |

## Still different (needs views lextrack doesn't have, or data the dump lacks)

- Views 0D and 16, plus mixer, scale, EQ, instrument list and live mode, aren't
  in lextrack. The mini-map letters stay as before because the dump has no
  label text to check them against.
- The transpose message, the random instrument combo, and editing with PLAY
  held aren't done yet.
- Screen layouts, the font and labels can't be checked because they're in
  data the dump doesn't include.
