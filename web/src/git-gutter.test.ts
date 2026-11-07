// The gutter's arithmetic, without a browser: a mark is a position, and the
// question every test asks is which line that position is on after an edit.

import { EditorState } from '@codemirror/state';
import { describe, expect, it } from 'vitest';

import type { LineMark } from './api';
import { GitMark, gitMarks, gitMarksFor, setGitMarks } from './git-gutter';

function stateWith(text: string, marks: LineMark[]): EditorState {
  return EditorState.create({ doc: text, extensions: [gitMarksFor(marks)] });
}

/** The marked lines of a state, in document order, with their letters. */
function marked(state: EditorState): { line: number; change: string }[] {
  const rows: { line: number; change: string }[] = [];
  const iter = state.field(gitMarks).iter();
  while (iter.value) {
    rows.push({
      line: state.doc.lineAt(iter.from).number,
      change: (iter.value as GitMark).change,
    });
    iter.next();
  }
  return rows;
}

describe('gitMarks', () => {
  it('places each mark on the line the daemon counted', () => {
    const state = stateWith('one\ntwo\nthree', [
      { line: 1, change: 'added' },
      { line: 3, change: 'deleted' },
    ]);
    expect(marked(state)).toEqual([
      { line: 1, change: 'added' },
      { line: 3, change: 'deleted' },
    ]);
  });

  it('moves a mark down when a line is inserted above it', () => {
    const state = stateWith('one\ntwo\nthree', [
      { line: 2, change: 'modified' },
      { line: 3, change: 'added' },
    ]);
    const inserted = state.update({ changes: { from: 0, insert: 'zero\n' } }).state;
    expect(marked(inserted)).toEqual([
      { line: 3, change: 'modified' },
      { line: 4, change: 'added' },
    ]);
  });

  it('moves a mark up when the text above it is removed', () => {
    const state = stateWith('one\ntwo\nthree', [{ line: 3, change: 'added' }]);
    // The first line and its newline: the mark belongs to "three", now line 2.
    const removed = state.update({ changes: { from: 0, to: 4 } }).state;
    expect(marked(removed)).toEqual([{ line: 2, change: 'added' }]);
  });

  it('stays on its line when the line itself is edited', () => {
    const state = stateWith('one\ntwo\nthree', [{ line: 2, change: 'modified' }]);
    const typed = state.update({ changes: { from: 5, insert: 'X' } }).state;
    expect(marked(typed)).toEqual([{ line: 2, change: 'modified' }]);
  });

  it('drops a line the document does not have rather than clamping it', () => {
    const state = stateWith('one\ntwo', [{ line: 9, change: 'added' }]);
    expect(marked(state)).toEqual([]);
  });

  it('replaces the whole set when a save answers with new marks', () => {
    const state = stateWith('one\ntwo\nthree', [
      { line: 1, change: 'added' },
      { line: 2, change: 'added' },
    ]);
    const written = state.update({
      effects: setGitMarks.of([{ line: 3, change: 'modified' }]),
    }).state;
    expect(marked(written)).toEqual([{ line: 3, change: 'modified' }]);
  });

  it('treats the marks in a save effect as being in the new document', () => {
    const state = stateWith('one\ntwo', []);
    const written = state.update({
      changes: { from: 0, insert: 'zero\n' },
      effects: setGitMarks.of([{ line: 2, change: 'added' }]),
    }).state;
    // Line 2 of the document the transaction produced is "one", not "two".
    expect(marked(written)).toEqual([{ line: 2, change: 'added' }]);
  });
});
