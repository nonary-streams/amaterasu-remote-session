namespace RemoteSessionUi {
  export type AnchorKind = 'physical' | 'client';
  export type MonitorEdge = 'left' | 'right' | 'above' | 'below';
  export type MonitorAlignment = 'start' | 'center' | 'end';
  export interface MonitorPlacement {
    anchor_kind: AnchorKind;
    anchor_id: string;
    edge: MonitorEdge;
    alignment: MonitorAlignment;
    gap_px: number;
    primary: boolean;
    preserve: boolean;
    automatic_fallback: boolean;
  }
  export interface MonitorLayout { version: 1; placements: Record<string, MonitorPlacement> }
  export interface MonitorClient { id: string; label: string; ready: boolean; state: string; error?: string }
  export interface MonitorAnchor { id: string; label: string }
  export interface MonitorApplicationStatus { state: string; error?: string }
  export interface LayoutReply {
    ok: true;
    revision: number;
    layout: MonitorLayout;
    clients: MonitorClient[];
    physical_anchors: MonitorAnchor[];
    application_status: MonitorApplicationStatus;
  }

  export function managementObject(value: unknown): Record<string, unknown> {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error('The monitor service returned an invalid document.');
    }
    return value as Record<string, unknown>;
  }
  export function managementText(value: unknown, maximum: number, allowEmpty = false): string {
    if (typeof value !== 'string' || (!allowEmpty && value.trim().length === 0)
      || new TextEncoder().encode(value).length > maximum || /[\u0000-\u001f\u007f]/u.test(value)) {
      throw new Error('The monitor service returned an invalid name or identity.');
    }
    return value;
  }
  export function managementRevision(value: unknown): number {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
      throw new Error('The document revision cannot be represented safely in this editor.');
    }
    return value;
  }
  export function managementInteger(value: unknown, minimum: number, maximum: number, message: string): number {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < minimum || value > maximum) {
      throw new Error(message);
    }
    return value;
  }
  function optionalBoolean(value: unknown): boolean {
    if (value !== undefined && typeof value !== 'boolean') throw new Error('The monitor placement contains an invalid switch.');
    return value === true;
  }
  export function parsePlacement(value: unknown): MonitorPlacement {
    const source = managementObject(value);
    if (source.anchor_kind !== 'physical' && source.anchor_kind !== 'client') throw new Error('Choose a display or client anchor.');
    if (!['left', 'right', 'above', 'below'].includes(String(source.edge))) throw new Error('Choose a monitor edge.');
    if (!['start', 'center', 'end'].includes(String(source.alignment))) throw new Error('Choose a monitor alignment.');
    const placement: MonitorPlacement = {
      anchor_kind: source.anchor_kind,
      anchor_id: managementText(source.anchor_id, 4096, true),
      edge: source.edge as MonitorEdge,
      alignment: source.alignment as MonitorAlignment,
      gap_px: managementInteger(source.gap_px, 0, 10_000, 'The monitor gap must be a whole number from 0 to 10000 pixels.'),
      primary: optionalBoolean(source.primary), preserve: optionalBoolean(source.preserve),
      automatic_fallback: optionalBoolean(source.automatic_fallback),
    };
    if (!placement.preserve && placement.anchor_id.length === 0) throw new Error('Choose an anchor for the relative position.');
    if (!placement.preserve) placement.automatic_fallback = false;
    return placement;
  }
  export function emptyLayout(): MonitorLayout { return { version: 1, placements: Object.create(null) as Record<string, MonitorPlacement> }; }
  export function automaticPlacement(): MonitorPlacement {
    return { anchor_kind: 'physical', anchor_id: '', edge: 'right', alignment: 'start',
      gap_px: 0, primary: false, preserve: true, automatic_fallback: true };
  }
  export function parseLayout(value: unknown): MonitorLayout {
    const source = managementObject(value);
    if (source.version !== 1) throw new Error('This monitor layout version is unsupported.');
    const placements = managementObject(source.placements);
    if (Object.keys(placements).length > 32) throw new Error('A monitor layout can contain at most 32 clients.');
    const result = emptyLayout();
    let primaryCount = 0;
    for (const id of Object.keys(placements).sort()) {
      managementText(id, 128);
      Object.defineProperty(result.placements, id, {
        enumerable: true, configurable: true, writable: true, value: parsePlacement(placements[id]),
      });
      primaryCount += Number(result.placements[id]!.primary);
    }
    if (primaryCount > 1) throw new Error('Choose only one primary monitor.');
    if (new TextEncoder().encode(JSON.stringify(result)).length > 12_000) throw new Error('The saved monitor layout is too large.');
    for (const id of Object.keys(result.placements)) {
      const visited = new Set<string>();
      let current = id;
      while (Object.hasOwn(result.placements, current)) {
        const placement = result.placements[current]!;
        if (placement.preserve || placement.anchor_kind === 'physical') break;
        if (visited.has(current)) throw new Error('Monitor anchors cannot form a loop.');
        visited.add(current);
        current = placement.anchor_id;
      }
    }
    return result;
  }
  export function layoutEqual(left: MonitorLayout, right: MonitorLayout): boolean {
    return JSON.stringify(parseLayout(left)) === JSON.stringify(parseLayout(right));
  }
  function parseUniqueList<T extends { id: string }>(value: unknown, parse: (value: unknown) => T): T[] {
    if (!Array.isArray(value) || value.length > 64) throw new Error('The monitor inventory exceeds this editor’s limit.');
    const entries = value.map(parse);
    if (new Set(entries.map(entry => entry.id)).size !== entries.length) throw new Error('The monitor inventory contains duplicate identities.');
    return entries;
  }
  export function parseLayoutReply(value: unknown): LayoutReply {
    const error = requestError(value);
    if (error) throw error;
    const source = managementObject(value);
    if (source.ok !== true) throw new Error('The layout service did not confirm success.');
    const clients = parseUniqueList(source.clients, value => {
      const client = managementObject(value);
      if (typeof client.ready !== 'boolean') throw new Error('The monitor readiness report is invalid.');
      return { id: managementText(client.id, 128), label: managementText(client.label, 160),
        ready: client.ready, state: managementText(client.state, 128),
        ...(client.error === undefined ? {} : { error: managementText(client.error, 1024) }) };
    });
    const anchors = parseUniqueList(source.physical_anchors, value => {
      const anchor = managementObject(value);
      return { id: managementText(anchor.id, 4096), label: managementText(anchor.label, 160) };
    });
    const status = managementObject(source.application_status);
    return { ok: true, revision: managementRevision(source.revision), layout: parseLayout(source.layout),
      clients, physical_anchors: anchors,
      application_status: { state: managementText(status.state, 128),
        ...(status.error === undefined ? {} : { error: managementText(status.error, 1024) }) } };
  }
  export function validateLayoutChoices(layout: MonitorLayout, inventory: LayoutReply): void {
    const clients = new Set(inventory.clients.map(client => client.id));
    const anchors = new Set(inventory.physical_anchors.map(anchor => anchor.id));
    for (const [id, placement] of Object.entries(layout.placements)) {
      if (!clients.has(id)) throw new Error('A saved position belongs to an unavailable client. Clear its saved rule before saving.');
      if (placement.preserve) continue;
      if (placement.anchor_kind === 'physical' ? !anchors.has(placement.anchor_id)
        : placement.anchor_id === id || !clients.has(placement.anchor_id)) {
        throw new Error('A monitor anchor is unavailable. Choose another anchor before saving.');
      }
    }
  }

  export class LayoutEditor {
    saved: LayoutReply | null = null;
    draft: MonitorLayout | null = null;
    busy = false;
    error = false;
    message = '';
    confirmationRequired = false;
    private disposed = false;
    private generation = 0;
    private inventoryGeneration = 0;
    private inventoryPending = false;

    constructor(private readonly api: SettingsApi, private readonly changed: () => void) {}
    dirty(): boolean {
      if (!this.saved || !this.draft) return false;
      try { return !layoutEqual(this.saved.layout, this.draft); }
      catch { return true; }
    }
    edit(clientId: string, placement: MonitorPlacement | null): void {
      if (this.disposed || this.busy || this.draft === null) return;
      const placements = Object.assign(Object.create(null), this.draft.placements) as Record<string, MonitorPlacement>;
      if (placement === null) delete placements[clientId];
      else {
        if (placement.primary) {
          for (const [id, current] of Object.entries(placements)) placements[id] = { ...current, primary: false };
        }
        Object.defineProperty(placements, clientId, { enumerable: true, configurable: true, writable: true, value: { ...placement } });
      }
      this.draft = { version: 1, placements };
      this.message = ''; this.error = false; this.changed();
    }
    validationError(): string | null {
      try {
        if (!this.draft || !this.saved) return 'Read the saved monitor layout first.';
        validateLayoutChoices(parseLayout(this.draft), this.saved);
        return null;
      } catch (error) { return errorMessage(error); }
    }
    async refresh(discardDraft = false): Promise<void> {
      if (this.disposed || this.busy) return;
      const generation = ++this.generation;
      this.inventoryGeneration++;
      this.busy = true; this.error = false; this.message = 'Reading saved monitor positions…'; this.changed();
      try {
        const current = parseLayoutReply(await this.api.call('layout'));
        if (!this.active(generation)) return;
        const placements = Object.assign(Object.create(null), current.layout.placements) as Record<string, MonitorPlacement>;
        if (!discardDraft && this.saved && this.draft) {
          const ids = new Set([...Object.keys(this.saved.layout.placements), ...Object.keys(this.draft.placements)]);
          for (const id of ids) {
            const previous = this.saved.layout.placements[id], draft = this.draft.placements[id];
            if (JSON.stringify(previous) === JSON.stringify(draft)) continue;
            if (draft === undefined) delete placements[id];
            else Object.defineProperty(placements, id, { enumerable: true, configurable: true, writable: true, value: { ...draft } });
          }
        }
        this.saved = current; this.draft = { version: 1, placements };
        this.confirmationRequired = false;
        this.message = this.dirty() ? 'Saved positions refreshed. Your unsaved changes are kept.' : 'Saved monitor positions are up to date.';
      } catch (error) {
        if (this.active(generation)) { this.message = errorMessage(error); this.error = true; }
      } finally {
        if (this.active(generation)) { this.busy = false; this.changed(); }
      }
    }
    async pollInventory(): Promise<void> {
      if (this.disposed || this.busy || this.inventoryPending || !this.saved) return;
      const generation = this.inventoryGeneration;
      this.inventoryPending = true;
      try {
        const current = parseLayoutReply(await this.api.call('layout'));
        if (this.disposed || generation !== this.inventoryGeneration || !this.saved) return;
        this.saved = { ...this.saved, clients: current.clients, physical_anchors: current.physical_anchors,
          application_status: current.application_status };
        this.changed();
      } catch {
        if (!this.disposed && generation === this.inventoryGeneration && this.saved) {
          this.saved = { ...this.saved, application_status: { state: 'unavailable', error: 'Current monitor status could not be refreshed.' } };
          this.changed();
        }
      } finally { this.inventoryPending = false; }
    }
    async save(): Promise<void> {
      if (this.disposed || this.busy || !this.saved || !this.draft || !this.dirty() || this.confirmationRequired) return;
      const invalid = this.validationError();
      if (invalid) { this.message = invalid; this.error = true; this.changed(); return; }
      const requested = parseLayout(this.draft), revision = this.saved.revision;
      if (!Number.isSafeInteger(revision + 1)) { this.message = 'The next layout revision cannot be represented safely.'; this.error = true; this.changed(); return; }
      const generation = ++this.generation;
      this.inventoryGeneration++;
      this.busy = true; this.error = false; this.message = 'Saving monitor positions…'; this.changed();
      let rejected = false;
      try {
        const result = await this.api.call('layout_update', { layout: requested, expected_revision: revision });
        if (!this.active(generation)) return;
        const rejection = requestError(result); rejected = rejection !== null;
        if (rejection) throw rejection;
        const updated = parseLayoutReply(result);
        if (updated.revision !== revision + 1 || !layoutEqual(updated.layout, requested)) throw new Error('The update did not confirm the requested layout.');
        const readback = parseLayoutReply(await this.api.call('layout'));
        if (!this.active(generation)) return;
        if (readback.revision !== updated.revision || !layoutEqual(readback.layout, requested)) throw new Error('The saved layout changed before readback.');
        this.saved = readback; this.draft = parseLayout(readback.layout);
        this.confirmationRequired = false; this.message = 'Monitor positions saved and confirmed.';
      } catch (error) {
        if (!this.active(generation)) return;
        this.error = true; this.message = errorMessage(error); this.confirmationRequired = !rejected;
        if (this.confirmationRequired) this.message += ' Refresh saved positions before saving again; your draft is kept.';
      } finally { if (this.active(generation)) { this.busy = false; this.changed(); } }
    }
    dispose(): void { this.disposed = true; this.generation++; this.inventoryGeneration++; }
    private active(generation: number): boolean { return !this.disposed && generation === this.generation; }
  }
}
