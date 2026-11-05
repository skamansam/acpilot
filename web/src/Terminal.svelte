<script lang="ts">
  import { onMount } from 'svelte';
  import { FitAddon } from '@xterm/addon-fit';
  import { WebglAddon } from '@xterm/addon-webgl';
  import { Terminal as XTerm } from '@xterm/xterm';
  import '@xterm/xterm/css/xterm.css';
  import type { TerminalSummary } from './api';
  import { attachPane, paneWhere, type PaneSocket } from './terminals';

  let {
    pane,
    token,
    onclose,
  }: { pane: TerminalSummary; token: string; onclose?: () => void } = $props();

  // xterm is the program's screen; this component owns the DOM node it draws into
  // and the socket that feeds it, and nothing else. The process belongs to the
  // daemon: detaching here leaves it running, which is the point of attaching.
  let host: HTMLDivElement;
  let status = $state<'connecting' | 'open' | 'reconnecting' | 'ended'>('connecting');

  onMount(() => {
    const styles = getComputedStyle(document.documentElement);
    const colour = (name: string, fallback: string): string =>
      styles.getPropertyValue(name).trim() || fallback;

    const terminal = new XTerm({
      // The page's palette, read rather than copied: a theme repaint is the page's
      // job, and a terminal with its own hardcoded colours would be a fourth place
      // a colour is written down.
      theme: {
        background: colour('--bg-panel', '#131c2e'),
        foreground: colour('--text', '#e6ecf7'),
        cursor: colour('--accent', '#5aa9ff'),
        selectionBackground: colour('--border', '#223049'),
      },
      fontFamily:
        "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace",
      fontSize: 13,
      cursorBlink: true,
      scrollback: 2000,
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(host);
    try {
      const webgl = new WebglAddon();
      // A lost context falls back to the DOM renderer rather than a blank pane: the
      // screen is a shell's output, and losing it is worse than drawing it slowly.
      webgl.onContextLoss(() => webgl.dispose());
      terminal.loadAddon(webgl);
    } catch {
      // No WebGL (a headless browser, a locked-down GPU): xterm's own renderer is
      // the correct fallback and is why the addon is optional.
    }
    fit.fit();

    // Copy and paste, on the keys a terminal uses. Ctrl+C must stay an interrupt,
    // which is why the copy chord carries Shift the way a terminal's does.
    terminal.attachCustomKeyEventHandler((event) => {
      if (event.type !== 'keydown' || !event.ctrlKey || !event.shiftKey) {
        return true;
      }
      if (event.key.toLowerCase() === 'c') {
        const selection = terminal.getSelection();
        if (selection) {
          void navigator.clipboard?.writeText(selection).catch(() => {});
        }
        return false;
      }
      if (event.key.toLowerCase() === 'v') {
        void navigator.clipboard
          ?.readText()
          .then((text) => terminal.paste(text))
          .catch(() => {});
        return false;
      }
      return true;
    });

    const socket: PaneSocket = attachPane({
      id: pane.id,
      token,
      write: (bytes) => terminal.write(bytes),
      attached: () => {
        // The daemon replays the screen after this notice, so the cells from the
        // previous socket go first — otherwise a reattach would draw the new
        // screen on top of the old one.
        terminal.reset();
        status = 'open';
      },
      exit: (code) => {
        status = 'ended';
        terminal.write(
          `\r\n[${code === undefined ? 'exited' : `exited with code ${code}`}]\r\n`,
        );
      },
      status: (next) => {
        if (next === 'reconnecting') {
          status = 'reconnecting';
        }
      },
    });

    // A pane's size is its cells, and the fit addon reads them from the element;
    // the program is told once, through the socket, because that is the only place
    // a live pane can say it.
    const observer =
      typeof ResizeObserver === 'undefined'
        ? null
        : new ResizeObserver(() => {
            fit.fit();
            socket.resize(terminal.cols, terminal.rows);
          });
    observer?.observe(host);

    return () => {
      observer?.disconnect();
      socket.close();
      terminal.dispose();
    };
  });

  const label = $derived(
    status === 'open'
      ? 'attached'
      : status === 'reconnecting'
        ? 'reconnecting…'
        : status === 'ended'
          ? 'exited'
          : 'connecting…',
  );
</script>

<div class="terminal">
  <div class="terminal-head">
    <span class="terminal-title">
      <code>{pane.command}</code>
      <span class="terminal-where">{paneWhere(pane)}</span>
    </span>
    <span class="terminal-status" data-status={status}>{label}</span>
    <button type="button" class="link" onclick={() => onclose?.()}>Detach</button>
  </div>
  <div class="terminal-screen" bind:this={host}></div>
</div>
