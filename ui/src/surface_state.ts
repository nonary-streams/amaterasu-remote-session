namespace RemoteSessionUi {
  export interface EditorGuard {
    update(dirty: boolean, busy: boolean): void;
    dispose(): void;
  }

  /** Independent editors share one shell navigation guard without clearing each other's state. */
  export class EditorGuards {
    private readonly editors = new Map<object, { dirty: boolean; busy: boolean }>();

    constructor(private readonly changed: (dirty: boolean, busy: boolean) => void) {}

    create(): EditorGuard {
      const key = {};
      let disposed = false;
      this.editors.set(key, { dirty: false, busy: false });
      return {
        update: (dirty, busy) => {
          if (disposed) return;
          this.editors.set(key, { dirty, busy });
          this.publish();
        },
        dispose: () => {
          if (disposed) return;
          disposed = true;
          this.editors.delete(key);
          this.publish();
        },
      };
    }

    private publish(): void {
      this.changed([...this.editors.values()].some(editor => editor.dirty),
        [...this.editors.values()].some(editor => editor.busy));
    }
  }
}
