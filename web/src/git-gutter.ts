// The gutter's git marks as positions rather than line numbers.
//
// The daemon counts what changed in lines of the file it read. The moment somebody
// types, those numbers belong to a document that no longer exists: every line above
// an edit moves, and a mark kept as a number would sit on a line that never changed.
// So a mark is stored at the start of the line it describes, and every transaction
// maps it through its changes; a save replaces the set with the answer git gave
// about the new text.

import {
  RangeSet,
  StateEffect,
  StateField,
  type Extension,
  type Range,
  type Text,
} from '@codemirror/state';
import { GutterMarker } from '@codemirror/view';

import type { LineChange, LineMark } from './api';
import { markLabel, markTitle } from './files';

/** One change, drawn as one character in the gutter. */
export class GitMark extends GutterMarker {
  /** What happened to the line; public so a test can name the mark it found. */
  readonly change: LineChange;

  constructor(change: LineChange) {
    super();
    this.change = change;
  }

  eq(other: GutterMarker): boolean {
    return other instanceof GitMark && other.change === this.change;
  }

  toDOM(): HTMLElement {
    const element = document.createElement('span');
    element.className = `cm-git-mark cm-git-${this.change}`;
    element.dataset.change = this.change;
    element.textContent = markLabel(this.change);
    element.title = markTitle(this.change);
    return element;
  }
}

/** A new set of marks, in the coordinates of the document the transaction makes. */
export const setGitMarks = StateEffect.define<LineMark[]>();

/**
 * The marks the daemon reported, placed at the start of their lines.
 *
 * A line number the document does not have is dropped rather than clamped: it is
 * an answer about a version this pane is not drawing, and a mark on the wrong line
 * is a worse claim than no mark.
 */
export function markRanges(doc: Text, marks: LineMark[]): RangeSet<GutterMarker> {
  const sorted = [...marks].sort((left, right) => left.line - right.line);
  const ranges: Range<GutterMarker>[] = [];
  let last = -1;
  for (const mark of sorted) {
    if (mark.line < 1 || mark.line > doc.lines) {
      continue;
    }
    const from = doc.line(mark.line).from;
    // Two answers about the same line are one cell: git should not send both, and
    // a duplicated range at one position would draw the letter twice.
    if (from === last) {
      continue;
    }
    last = from;
    ranges.push(new GitMark(mark.change).range(from));
  }
  return RangeSet.of(ranges, true);
}

/**
 * The marks, carried through every edit.
 *
 * Mapping is what makes the gutter stay true while typing: an insertion above a
 * mark moves it down with its line, and a deletion above it moves it up. Effects
 * are replayed against `transaction.state.doc` because their line numbers were
 * counted in the document after the change, not the one before it.
 */
export const gitMarks = StateField.define<RangeSet<GutterMarker>>({
  create: () => RangeSet.empty,
  update(value, transaction) {
    let next = transaction.docChanged ? value.map(transaction.changes) : value;
    for (const effect of transaction.effects) {
      if (effect.is(setGitMarks)) {
        next = markRanges(transaction.state.doc, effect.value);
      }
    }
    return next;
  },
});

/** The field for one document, initialised with the marks it was read with. */
export function gitMarksFor(marks: LineMark[]): Extension {
  return gitMarks.init((state) => markRanges(state.doc, marks));
}
