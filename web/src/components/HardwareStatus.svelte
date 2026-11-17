<script lang="ts">
    import { afterUpdate, onDestroy, onMount } from "svelte";
    import Icon from "@iconify/svelte";
    import {
        DefaultService,
        type ExpansionBayInfo,
        type HardwareInfo,
        type InputDeckInfo,
        type IntrusionInfo,
        type PdPortInfo,
        type PrivacyInfo,
    } from "../api";
    import HardwareGroupCard, { type Group, type Row, type Tone } from "./HardwareGroupCard.svelte";
    import HardwarePortCard from "./HardwarePortCard.svelte";

    // Read-only views of what the EC reports: USB-C ports, expansion bay, input deck, privacy
    // switches and chassis. Sections the machine doesn't have are absent from the response; the
    // whole card stays hidden when there are none.
    //
    // On a model with a known layout the cards sit around a photo of the laptop, each joined to
    // the place it describes. Other models get the same cards in plain grids.

    const COLLAPSED_KEY = "fc.hardware.collapsed";
    const POLL_MS = 3000;

    type GroupKey = "bay" | "deck" | "privacy" | "chassis";
    /** A place on the photo, in percent of the visible (cropped) picture. */
    type Spot = { key: string; side: "left" | "right"; x: number; y: number };
    type LaptopMap = {
        image: string;
        alt: string;
        /** Crops the photo to the laptop; the spots are relative to this crop. */
        imageStyle: string;
        /** Port index -> position name. */
        positions: string[];
        /** Top to bottom on each side, in the order the spots appear on the photo. */
        spots: Spot[];
    };
    type MapItem = Spot & { tone: Tone; label: string; port?: PdPortInfo; group?: Group };
    type Line = { key: string; tone: Tone; x1: number; y1: number; x2: number; y2: number };

    // laptop-16.jpg is 2427x1618. The visible crop is the 1860x1395 region at (440, 110).
    // The left-side ports are visible in the photo; the right-side ones are on the far side,
    // so their spots sit on the right edge of the deck above them.
    const FRAMEWORK_16: LaptopMap = {
        image: `${import.meta.env.BASE_URL}assets/laptop-16.jpg`,
        alt: "Framework Laptop 16",
        imageStyle: "width: 130.48%; left: -23.66%; top: -7.885%;",
        positions: ["Right back", "Right middle", "Left middle", "Left back"],
        spots: [
            { key: "privacy", side: "left", x: 31.9, y: 7.0 },
            { key: "deck", side: "left", x: 48.1, y: 74.8 },
            { key: "port3", side: "left", x: 17.7, y: 79.3 },
            { key: "port2", side: "left", x: 21.4, y: 82.5 },
            { key: "bay", side: "right", x: 60.2, y: 62.7 },
            { key: "port0", side: "right", x: 71.3, y: 69.0 },
            { key: "port1", side: "right", x: 75.6, y: 71.5 },
            { key: "chassis", side: "right", x: 83.9, y: 80.3 },
        ],
    };

    // The photo is light in every theme and the lines also cross the card background, so both use
    // fixed contrasts instead of theme colours: a white ring around each marker, a slate fill for
    // the ones without a state colour, and light lines over a dark casing.
    const SPOT_CLASS: Record<Tone, string> = {
        ok: "bg-success",
        info: "bg-info",
        warn: "bg-warning",
        error: "bg-error",
        idle: "bg-slate-400",
        plain: "bg-slate-700",
    };
    const LINE_CLASS: Record<Tone, string> = {
        ok: "text-success",
        info: "text-info",
        warn: "text-warning",
        error: "text-error",
        idle: "text-white opacity-60",
        plain: "text-white",
    };

    const FRAMEWORK_13_POSITIONS =["Right back", "Right front", "Left front", "Left back"];

    let info: HardwareInfo | null = null;
    let model = "";
    let collapsed = readCollapsed();
    let pollId: ReturnType<typeof setInterval> | null = null;
    let inFlight = false;

    function readCollapsed(): boolean {
        try {
            return localStorage.getItem(COLLAPSED_KEY) === "1";
        } catch {
            return false;
        }
    }

    function toggleCollapsed() {
        collapsed = !collapsed;
        try {
            localStorage.setItem(COLLAPSED_KEY, collapsed ? "1" : "0");
        } catch {}
        if (!collapsed) void refresh();
    }

    async function refresh() {
        if (inFlight) return;
        inFlight = true;
        try {
            info = await DefaultService.getHardware();
        } catch {
            // Keep the last reading; the header already shows when the service is unreachable.
        } finally {
            inFlight = false;
        }
    }

    function tick() {
        // Nothing to update while the card is collapsed or the tab is in the background.
        if (collapsed || document.hidden) return;
        void refresh();
    }

    onMount(() => {
        // The first read also decides whether the card is shown at all, so it runs when collapsed too.
        void refresh();
        pollId = setInterval(tick, POLL_MS);
        DefaultService.getVersions()
            .then((v) => (model = v.mainboard_type ?? ""))
            .catch(() => {});
    });

    onDestroy(() => {
        if (pollId) clearInterval(pollId);
        if (frame) cancelAnimationFrame(frame);
    });

    $: ports = info?.pd_ports ?? [];
    $: groups = buildGroups(info);
    $: groupList = GROUP_ORDER.flatMap((key) => {
        const group = groups[key];
        return group ? [group] : [];
    });
    $: hasAny = ports.length > 0 || groupList.length > 0;
    $: laptopMap = model.toLowerCase().includes("laptop 16") ? FRAMEWORK_16 : null;
    $: positions = laptopMap?.positions ?? plainPositions(model);
    $: mapItems = laptopMap ? buildMapItems(laptopMap, ports, groups) : [];
    $: leftItems = mapItems.filter((item) => item.side === "left");
    $: rightItems = mapItems.filter((item) => item.side === "right");

    /** Port positions for models without a photo map. */
    function plainPositions(name: string): string[] | null {
        const m = name.toLowerCase();
        if (m.includes("laptop 13") || m === "laptop" || m.startsWith("laptop (")) {
            return FRAMEWORK_13_POSITIONS;
        }
        return null;
    }

    function portTone(p: PdPortInfo): Tone {
        if (!p.connected) return "idle";
        if (p.power_role === "Sink") return "ok";
        if (p.power_role === "Source") return "info";
        return "plain";
    }

    /** A card takes the colour of its most serious row. */
    function groupTone(group: Group): Tone {
        if (group.rows.some((row) => row.tone === "error")) return "error";
        if (group.rows.some((row) => row.tone === "warn")) return "warn";
        return "plain";
    }

    function buildMapItems(
        map: LaptopMap,
        portList: PdPortInfo[],
        byKey: Partial<Record<GroupKey, Group>>,
    ): MapItem[] {
        const out: MapItem[] = [];
        for (const spot of map.spots) {
            if (spot.key.startsWith("port")) {
                const index = Number(spot.key.slice(4));
                const port = portList.find((p) => p.port === index);
                if (!port) continue;
                const where = map.positions[index] ?? "";
                out.push({
                    ...spot,
                    tone: portTone(port),
                    label: `USB-C ${index}${where ? ` (${where.toLowerCase()})` : ""}`,
                    port,
                });
            } else {
                const group = byKey[spot.key as GroupKey];
                if (!group) continue;
                out.push({ ...spot, tone: groupTone(group), label: group.title, group });
            }
        }
        return out;
    }

    // ---- leader lines from each card to its spot on the photo ---------------------------------

    let mapEl: HTMLDivElement | undefined;
    let photoEl: HTMLDivElement | undefined;
    const anchors = new Map<string, HTMLElement>();
    let lines: Line[] = [];
    let linesKey = "";
    let frame = 0;

    function scheduleMeasure() {
        if (frame) return;
        frame = requestAnimationFrame(() => {
            frame = 0;
            measure();
        });
    }

    function measure() {
        const next: Line[] = [];
        if (mapEl && photoEl) {
            const m = mapEl.getBoundingClientRect();
            const p = photoEl.getBoundingClientRect();
            const round = (v: number) => Math.round(v * 2) / 2;
            for (const item of mapItems) {
                const el = anchors.get(item.key);
                if (!el || p.width < 1) continue;
                const r = el.getBoundingClientRect();
                // Only when the card sits beside the photo; stacked (narrow) layouts get no lines.
                let x1: number;
                if (r.right <= p.left) x1 = r.right;
                else if (r.left >= p.right) x1 = r.left;
                else continue;
                next.push({
                    key: item.key,
                    tone: item.tone,
                    x1: round(x1 - m.left),
                    y1: round(r.top - m.top + r.height / 2),
                    x2: round(p.left - m.left + (p.width * item.x) / 100),
                    y2: round(p.top - m.top + (p.height * item.y) / 100),
                });
            }
        }
        // Assigning only on change keeps afterUpdate from re-triggering itself.
        const key = JSON.stringify(next);
        if (key !== linesKey) {
            linesKey = key;
            lines = next;
        }
    }

    function anchor(node: HTMLElement, key: string) {
        anchors.set(key, node);
        scheduleMeasure();
        return {
            update(nextKey: string) {
                if (anchors.get(key) === node) anchors.delete(key);
                key = nextKey;
                anchors.set(key, node);
                scheduleMeasure();
            },
            destroy() {
                if (anchors.get(key) === node) anchors.delete(key);
            },
        };
    }

    function watchSize(node: HTMLElement) {
        const observer = new ResizeObserver(scheduleMeasure);
        observer.observe(node);
        return {
            destroy() {
                observer.disconnect();
            },
        };
    }

    afterUpdate(scheduleMeasure);

    // ---- what the groups say -------------------------------------------------------------------

    const GROUP_ORDER: GroupKey[] = ["bay", "deck", "privacy", "chassis"];

    // The service passes the EC's own names through; these make the common ones readable and
    // leave anything unknown as it is.
    const BAY_VENDOR: Record<string, string> = {
        NvidiaGn22: "NVIDIA GPU (GN22)",
        GpuAmdR23M: "AMD GPU (R23M)",
        FanOnly: "Fan module",
        SsdHolder: "SSD holder",
        PcieAccessory: "PCIe accessory",
        Initializing: "Initializing",
    };
    const BAY_BOARD: Record<string, string> = {
        DualInterposer: "Dual interposer",
        SingleInterposer: "Single interposer",
        UmaFans: "Fan module",
        NoModule: "No module",
    };
    const BAY_PCIE: Record<string, string> = {
        Pcie8x1: "8 lanes × 1",
        Pcie4x1: "4 lanes × 1",
        Pcie4x2: "4 lanes × 2",
    };

    /** "KeyboardA" -> "Keyboard A", "TurningOn" -> "Turning On" */
    function spaced(name: string): string {
        return name.replace(/([a-z0-9])([A-Z])/g, "$1 $2");
    }

    function times(n: number): string {
        return n === 1 ? "1 time" : `${n} times`;
    }

    function bayGroup(bay: ExpansionBayInfo): Group {
        const rows: Row[] = [];
        if (bay.vendor) {
            rows.push({ label: "Module", value: BAY_VENDOR[bay.vendor] ?? bay.vendor });
        }
        rows.push({
            label: "Board",
            value: BAY_BOARD[bay.board] ?? bay.board,
            tone: bay.board.startsWith("BadConnection") ? "error" : undefined,
        });
        if (bay.pcie_config) {
            rows.push({ label: "PCIe", value: BAY_PCIE[bay.pcie_config] ?? bay.pcie_config });
        }
        if (bay.serial) rows.push({ label: "Serial", value: bay.serial });
        rows.push({
            label: "Status",
            value: bay.fault ? "Fault" : bay.enabled ? "Enabled" : "Disabled",
            tone: bay.fault ? "error" : bay.enabled ? "ok" : undefined,
        });
        rows.push({
            label: "Door",
            value: bay.door_closed ? "Closed" : "Open",
            tone: bay.door_closed ? undefined : "warn",
        });
        return { title: "Expansion bay", icon: "hugeicons:gpu", rows };
    }

    function deckGroup(deck: InputDeckInfo): Group {
        const rows: Row[] = [
            {
                label: "State",
                value: spaced(deck.state),
                tone: deck.state === "On" ? "ok" : deck.state === "Disconnected" ? "warn" : undefined,
            },
            {
                label: "Touchpad",
                value: deck.touchpad_present ? "Present" : "Missing",
                tone: deck.touchpad_present ? undefined : "warn",
            },
        ];
        (deck.top_row ?? []).forEach((module, pos) => {
            // A wide module covers the positions next to it, which then read as disconnected.
            if (module === "Disconnected") return;
            rows.push({ label: `Position ${pos}`, value: spaced(module) });
        });
        return { title: "Input deck", icon: "mdi:keyboard-outline", rows };
    }

    function privacyGroup(privacy: PrivacyInfo): Group {
        const row = (label: string, connected: boolean): Row => ({
            label,
            value: connected ? "Connected" : "Cut off",
            tone: connected ? undefined : "warn",
        });
        return {
            title: "Privacy switches",
            icon: "mdi:shield-lock-outline",
            rows: [
                row("Microphone", privacy.microphone_connected),
                row("Camera", privacy.camera_connected),
            ],
        };
    }

    function chassisGroup(chassis: IntrusionInfo): Group {
        return {
            title: "Chassis",
            icon: "mdi:laptop",
            rows: [
                {
                    label: "Now",
                    value: chassis.currently_open ? "Open" : "Closed",
                    tone: chassis.currently_open ? "warn" : undefined,
                },
                {
                    label: "Opened",
                    value:
                        chassis.total_opened > 0
                            ? times(chassis.total_opened)
                            : chassis.ever_opened
                              ? "Yes"
                              : "Never",
                },
                {
                    label: "Opened while off",
                    value: chassis.opened_while_off > 0 ? times(chassis.opened_while_off) : "Never",
                },
                {
                    label: "Coin cell removed",
                    value: chassis.coin_cell_ever_removed ? "Yes" : "Never",
                },
            ],
        };
    }

    function buildGroups(hw: HardwareInfo | null): Partial<Record<GroupKey, Group>> {
        const out: Partial<Record<GroupKey, Group>> = {};
        if (!hw) return out;
        if (hw.expansion_bay) out.bay = bayGroup(hw.expansion_bay);
        if (hw.input_deck) out.deck = deckGroup(hw.input_deck);
        if (hw.privacy) out.privacy = privacyGroup(hw.privacy);
        if (hw.intrusion) out.chassis = chassisGroup(hw.intrusion);
        return out;
    }
</script>

<svelte:window on:resize={scheduleMeasure} />

{#if hasAny}
    <div class="card bg-base-100 shadow">
        <div class="flex flex-col p-2">
            <div class="flex items-center justify-between gap-2 pl-3">
                <h2 class="card-title pb-0.5">Hardware</h2>
                <button
                    type="button"
                    class="btn btn-ghost btn-xs"
                    aria-expanded={!collapsed}
                    aria-controls="hardware-status-body"
                    aria-label={collapsed ? "Show hardware status" : "Hide hardware status"}
                    title={collapsed ? "Show" : "Hide"}
                    on:click={toggleCollapsed}
                >
                    <Icon
                        icon={collapsed ? "mdi:chevron-down" : "mdi:chevron-up"}
                        class="text-base"
                    />
                </button>
            </div>
            {#if !collapsed}
                <div id="hardware-status-body" class="mt-1">
                    {#if laptopMap && mapItems.length > 0}
                        <div class="hw-map" bind:this={mapEl} use:watchSize>
                            <svg class="hw-lines" aria-hidden="true">
                                <!-- A dark casing under each line keeps it readable across the light photo. -->
                                {#each lines as line (line.key)}
                                    <line
                                        x1={line.x1}
                                        y1={line.y1}
                                        x2={line.x2}
                                        y2={line.y2}
                                        stroke="currentColor"
                                        stroke-width="4"
                                        stroke-linecap="round"
                                        class="text-black opacity-50"
                                    />
                                {/each}
                                {#each lines as line (line.key)}
                                    <line
                                        x1={line.x1}
                                        y1={line.y1}
                                        x2={line.x2}
                                        y2={line.y2}
                                        stroke="currentColor"
                                        stroke-width="1.5"
                                        stroke-linecap="round"
                                        class={LINE_CLASS[line.tone]}
                                    />
                                {/each}
                            </svg>
                            <div class="flex flex-col gap-2 min-w-0">
                                {#each leftItems as item (item.key)}
                                    <div class="min-w-0" use:anchor={item.key}>
                                        {#if item.port}
                                            <HardwarePortCard
                                                port={item.port}
                                                position={positions?.[item.port.port] ?? null}
                                                tone={item.tone}
                                                dot
                                            />
                                        {:else if item.group}
                                            <HardwareGroupCard group={item.group} tone={item.tone} dot />
                                        {/if}
                                    </div>
                                {/each}
                            </div>
                            <div
                                class="hw-photo rounded-box border surface-border"
                                bind:this={photoEl}
                                use:watchSize
                            >
                                <img
                                    src={laptopMap.image}
                                    alt={laptopMap.alt}
                                    style={laptopMap.imageStyle}
                                    draggable="false"
                                />
                                {#each mapItems as item (item.key)}
                                    <span
                                        class="hw-spot ring-2 ring-white shadow-md {SPOT_CLASS[item.tone]}"
                                        style="left: {item.x}%; top: {item.y}%;"
                                        title={item.label}
                                    ></span>
                                {/each}
                            </div>
                            <div class="flex flex-col gap-2 min-w-0">
                                {#each rightItems as item (item.key)}
                                    <div class="min-w-0" use:anchor={item.key}>
                                        {#if item.port}
                                            <HardwarePortCard
                                                port={item.port}
                                                position={positions?.[item.port.port] ?? null}
                                                tone={item.tone}
                                                dot
                                            />
                                        {:else if item.group}
                                            <HardwareGroupCard group={item.group} tone={item.tone} dot />
                                        {/if}
                                    </div>
                                {/each}
                            </div>
                        </div>
                    {:else}
                        <div class="flex flex-col gap-2">
                            {#if ports.length > 0}
                                <div
                                    class="grid gap-2 grid-cols-1 sm:grid-cols-2 lg:grid-cols-4"
                                    role="list"
                                    aria-label="USB-C ports"
                                >
                                    {#each ports as port (port.port)}
                                        <div class="min-w-0" role="listitem">
                                            <HardwarePortCard
                                                {port}
                                                position={positions?.[port.port] ?? null}
                                                tone={portTone(port)}
                                            />
                                        </div>
                                    {/each}
                                </div>
                            {/if}
                            {#if groupList.length > 0}
                                <div class="grid gap-2 grid-cols-1 sm:grid-cols-2 lg:grid-cols-4">
                                    {#each groupList as group (group.title)}
                                        <HardwareGroupCard {group} tone={groupTone(group)} />
                                    {/each}
                                </div>
                            {/if}
                        </div>
                    {/if}
                </div>
            {/if}
        </div>
    </div>
{/if}

<style>
    /* Narrow: the photo, then the cards. Wide: cards on both sides of the photo, joined by lines. */
    .hw-map {
        position: relative;
        display: grid;
        grid-template-columns: minmax(0, 1fr);
        gap: 0.5rem;
    }

    .hw-photo {
        position: relative;
        order: -1;
        overflow: hidden;
        aspect-ratio: 4 / 3;
        width: 100%;
        max-width: 32rem;
        justify-self: center;
    }

    .hw-photo img {
        position: absolute;
        max-width: none;
        user-select: none;
    }

    .hw-spot {
        position: absolute;
        width: 0.625rem;
        height: 0.625rem;
        border-radius: 9999px;
        transform: translate(-50%, -50%);
        z-index: 20;
    }

    .hw-lines {
        display: none;
        position: absolute;
        inset: 0;
        width: 100%;
        height: 100%;
        pointer-events: none;
        z-index: 10;
    }

    @media (min-width: 1024px) {
        .hw-map {
            grid-template-columns: minmax(0, 1fr) minmax(0, 1.7fr) minmax(0, 1fr);
            column-gap: 2rem;
            align-items: center;
        }

        .hw-photo {
            order: 0;
            max-width: none;
        }

        .hw-lines {
            display: block;
        }
    }
</style>
