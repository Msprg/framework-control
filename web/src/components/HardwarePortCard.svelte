<script lang="ts">
    import Icon from "@iconify/svelte";
    import type { PdPortInfo } from "../api";
    import { TONE_BG, type Tone } from "./HardwareGroupCard.svelte";

    export let port: PdPortInfo;
    /** Where the port sits on the chassis, when the layout of this model is known. */
    export let position: string | null = null;
    export let tone: Tone = "plain";
    /** Show the dot that matches this card's marker on the laptop photo. */
    export let dot = false;

    $: watts = portWatts(port);
    $: badges = portBadges(port);

    function portWatts(p: PdPortInfo): number | null {
        if (p.voltage_mv == null || p.current_ma == null) return null;
        return (p.voltage_mv * p.current_ma) / 1_000_000;
    }

    function fmtWatts(w: number): string {
        return w >= 100 ? w.toFixed(0) : w.toFixed(1);
    }

    function trimZeros(n: number, digits: number): string {
        return n.toFixed(digits).replace(/\.?0+$/, "");
    }

    function portDetail(p: PdPortInfo): string {
        const parts: string[] = [];
        if (p.voltage_mv != null) parts.push(`${trimZeros(p.voltage_mv / 1000, 2)} V`);
        if (p.current_ma != null) parts.push(`${trimZeros(p.current_ma / 1000, 2)} A`);
        return parts.join(" · ");
    }

    function portDirection(p: PdPortInfo): string {
        if (p.power_role === "Sink") return "Power in";
        if (p.power_role === "Source") return "Power out";
        return "Connected";
    }

    function portBadges(p: PdPortInfo): { text: string; title: string }[] {
        const out: { text: string; title: string }[] = [];
        if (p.sink_active) {
            // The active input, whether or not the battery is charging from it right now.
            out.push({ text: "Power source", title: "The system takes its power from this port" });
        }
        if (p.pd_contract) {
            out.push({
                text: p.epr_active ? "PD EPR" : "PD",
                title: p.epr_active
                    ? "USB Power Delivery contract, Extended Power Range active"
                    : "USB Power Delivery contract",
            });
        }
        if ((p.dp_alt_mode ?? []).length > 0) {
            out.push({
                text: "DisplayPort",
                title: `DisplayPort alt mode: ${(p.dp_alt_mode ?? []).join(", ")}`,
            });
        }
        if (p.data_role === "Dfp") {
            out.push({ text: "Host", title: "Data role: downstream-facing port (host)" });
        } else if (p.data_role === "Ufp") {
            out.push({ text: "Device", title: "Data role: upstream-facing port (device)" });
        }
        return out;
    }

    function portTitle(p: PdPortInfo): string {
        const lines: string[] = [];
        if (p.partner) lines.push(`Port partner: ${p.partner}`);
        if (p.cc_polarity) lines.push(`CC polarity: ${p.cc_polarity}`);
        if (p.vconn != null) lines.push(`VCONN: ${p.vconn ? "on" : "off"}`);
        if (p.epr_supported != null) {
            lines.push(`EPR: ${p.epr_supported ? "supported" : "not supported"}`);
        }
        return lines.join("\n");
    }
</script>

<div
    class="bg-base-200 rounded-box border surface-border px-3 py-2 min-w-0"
    class:opacity-70={!port.connected}
    title={portTitle(port)}
>
    <div class="flex items-center gap-2 text-xs">
        {#if dot}
            <span class="w-2 h-2 rounded-full shrink-0 {TONE_BG[tone]}"></span>
        {/if}
        <Icon icon="mdi:usb-c-port" class="w-4 h-4 shrink-0" />
        <span class="font-medium whitespace-nowrap">USB-C {port.port}</span>
        {#if position}
            <span class="opacity-60 ml-auto truncate">{position}</span>
        {/if}
    </div>
    {#if port.connected}
        <div class="flex flex-wrap items-baseline gap-x-2 mt-1">
            {#if watts != null && watts > 0}
                <span
                    class="text-lg font-semibold tabular-nums"
                    title="Negotiated limit, not the measured power">{fmtWatts(watts)} W</span
                >
            {/if}
            <span class="text-xs opacity-70 whitespace-nowrap">{portDirection(port)}</span>
            <span class="text-xs opacity-70 tabular-nums whitespace-nowrap ml-auto"
                >{portDetail(port)}</span
            >
        </div>
        {#if badges.length > 0}
            <div class="flex flex-wrap gap-1 mt-1">
                {#each badges as badge (badge.text)}
                    <span class="badge badge-ghost badge-sm" title={badge.title}>{badge.text}</span>
                {/each}
            </div>
        {/if}
    {:else}
        <div class="text-xs opacity-70 mt-1">Nothing connected</div>
    {/if}
</div>

<style>
    .tabular-nums {
        font-variant-numeric: tabular-nums;
    }
</style>
