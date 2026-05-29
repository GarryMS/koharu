'use client'

import { useDrag } from '@use-gesture/react'
import { useMemo, useRef } from 'react'
import { useHotkeys } from 'react-hotkeys-hook'

import { useBlobImage } from '@/hooks/useBlobData'
import { useCurrentPage, useTextNodes, type TextNodeEntry } from '@/hooks/useCurrentPage'
import type { NodeDataPatch, Transform } from '@/lib/api/schemas'
import { applyOp, queueAutoRender } from '@/lib/io/scene'
import { ops } from '@/lib/ops'
import { useEditorUiStore } from '@/lib/stores/editorUiStore'
import { useSelectionStore } from '@/lib/stores/selectionStore'

type TextBlockLayerProps = {
  showSprites?: boolean
  scale: number
  style?: React.CSSProperties
}

/**
 * Overlay for the active page's Text nodes. Each rectangle is draggable /
 * resizable; commits dispatch `Op::UpdateNode { transform }` through
 * `applyCommand`. Selection is driven by `selectionStore.nodeIds`.
 */
export function TextBlockLayer({ showSprites, scale, style }: TextBlockLayerProps) {
  const nodes = useTextNodes()
  const page = useCurrentPage()
  const selectedIds = useSelectionStore((s) => s.nodeIds)
  const select = useSelectionStore((s) => s.select)
  const mode = useEditorUiStore((s) => s.mode)
  const interactive = mode === 'select' || mode === 'block'

  const hasSelection = useMemo(() => {
    for (const id of selectedIds) if (id) return true
    return false
  }, [selectedIds])

  const removeNode = async (id: string) => {
    if (!page) return
    const node = page.nodes[id]
    if (!node) return
    const idx = Object.keys(page.nodes).indexOf(id)
    await applyOp(ops.removeNode(page.id, id, node, idx < 0 ? 0 : idx))
    if ('text' in node.kind) queueAutoRender(page.id)
  }

  const removeSelected = async () => {
    if (!page) return
    // Snapshot selection now: each op invalidates the page state by removing a
    // node, so we can't iterate against a stale closure mid-loop.
    const ids = Array.from(selectedIds).filter((id): id is string => !!id)
    for (const id of ids) {
      await removeNode(id)
    }
  }

  const updateTransform = async (id: string, t: Transform) => {
    if (!page) return
    const data: NodeDataPatch = {
      text: {
        lockLayoutBox: true,
      },
    }
    await applyOp(ops.updateNode(page.id, id, { transform: t, data }))
    queueAutoRender(page.id)
  }

  useHotkeys(
    'delete',
    () => {
      if (hasSelection && interactive) void removeSelected()
    },
    { enabled: hasSelection && interactive },
    [selectedIds, interactive],
  )

  return (
    <div
      data-text-block-layer
      style={{
        ...style,
        position: 'absolute',
        inset: 0,
        width: '100%',
        height: '100%',
        pointerEvents: 'none',
      }}
    >
      {showSprites &&
        nodes.map((n, i) => <BlockSprite key={`sprite-${n.id ?? i}`} node={n} scale={scale} />)}
      {nodes.map((n, i) => (
        <TextBlockItem
          key={n.id}
          node={n}
          index={i}
          scale={scale}
          selected={selectedIds.has(n.id)}
          interactive={interactive}
          onSelect={(id, additive) => select(id, additive)}
          onCommit={(t) => void updateTransform(n.id, t)}
        />
      ))}
    </div>
  )
}

type TextBlockItemProps = {
  node: TextNodeEntry
  index: number
  scale: number
  selected: boolean
  interactive: boolean
  onSelect: (id: string, additive: boolean) => void
  onCommit: (transform: Transform) => void
}

const isAdditiveEvent = (event: unknown): boolean => {
  if (!event || typeof event !== 'object') return false
  const e = event as { shiftKey?: boolean; metaKey?: boolean; ctrlKey?: boolean }
  return !!(e.shiftKey || e.metaKey || e.ctrlKey)
}

const isCtrlEvent = (event: unknown): boolean => {
  if (!event || typeof event !== 'object') return false
  return !!(event as { ctrlKey?: boolean }).ctrlKey
}

const RESIZE_HANDLE_SIZE = 8
const ROTATE_HANDLE_DISTANCE = 26
const ROTATE_HANDLE_SIZE = 12

type ResizeEdge = { top: boolean; bottom: boolean; left: boolean; right: boolean }
type Point = { x: number; y: number }

function TextBlockItem({
  node,
  index,
  scale,
  selected,
  interactive,
  onSelect,
  onCommit,
}: TextBlockItemProps) {
  const boxRef = useRef<HTMLDivElement>(null)
  const dragStart = useRef({ x: 0, y: 0, w: 0, h: 0 })
  const rotateCenter = useRef<Point | null>(null)
  const edgeRef = useRef<ResizeEdge | null>(null)
  const isResizeRef = useRef(false)

  const setBox = (x: number, y: number, w: number, h: number, rotationDeg: number) => {
    const el = boxRef.current
    if (!el) return
    el.style.left = `${x}px`
    el.style.top = `${y}px`
    el.style.transform = `rotate(${rotationDeg}deg)`
    el.style.width = `${w}px`
    el.style.height = `${h}px`
  }

  const t = node.transform

  const bind = useDrag(
    ({ first, last, movement: [mx, my], event, tap }) => {
      if (!interactive) return
      if (isCtrlEvent(event) && !tap) {
        if (last) {
          isResizeRef.current = false
          edgeRef.current = null
        }
        return
      }
      event?.stopPropagation()
      const additive = isAdditiveEvent(event)
      if (tap) {
        onSelect(node.id, additive)
        return
      }
      if (first) {
        dragStart.current = {
          x: t.x * scale,
          y: t.y * scale,
          w: t.width * scale,
          h: t.height * scale,
        }
        // Keep multi-selection intact when dragging a node that's already selected;
        // otherwise this click is a single-select (unless the modifier is held).
        if (additive || !selected) onSelect(node.id, additive)
      }
      const { x: sx, y: sy, w: sw, h: sh } = dragStart.current
      const edge = edgeRef.current
      if (isResizeRef.current && edge) {
        let dx = 0
        let dy = 0
        let w = sw
        let h = sh
        if (edge.right) w += mx
        if (edge.left) {
          w -= mx
          dx = mx
        }
        if (edge.bottom) h += my
        if (edge.top) {
          h -= my
          dy = my
        }
        w = Math.max(4 * scale, w)
        h = Math.max(4 * scale, h)
        if (edge.left && w === 4 * scale) dx = sw - 4 * scale
        if (edge.top && h === 4 * scale) dy = sh - 4 * scale
        setBox(sx + dx, sy + dy, w, h, t.rotationDeg ?? 0)
        if (last) {
          isResizeRef.current = false
          edgeRef.current = null
          onCommit({
            x: Math.round((sx + dx) / scale),
            y: Math.round((sy + dy) / scale),
            width: Math.max(4, Math.round(w / scale)),
            height: Math.max(4, Math.round(h / scale)),
            rotationDeg: t.rotationDeg ?? 0,
          })
        }
      } else {
        setBox(sx + mx, sy + my, sw, sh, t.rotationDeg ?? 0)
        if (last) {
          onCommit({
            x: Math.round((sx + mx) / scale),
            y: Math.round((sy + my) / scale),
            width: t.width,
            height: t.height,
            rotationDeg: t.rotationDeg ?? 0,
          })
        }
      }
    },
    {
      pointer: { buttons: 1, touch: true },
      filterTaps: true,
      preventDefault: true,
      eventOptions: { passive: false },
    },
  )

  const rotateBind = useDrag(
    ({ first, last, event }) => {
      if (!interactive || !selected) return
      if (isCtrlEvent(event)) return
      event?.stopPropagation()
      if (event?.cancelable) event.preventDefault()
      if (first) {
        onSelect(node.id, false)
        rotateCenter.current = getElementCenter(boxRef.current)
      }

      const center = rotateCenter.current
      const pointer = getPointer(event)
      if (!center || !pointer) return

      const raw = angleFromCenter(center, pointer)
      const snapped = isShiftEvent(event) ? Math.round(raw / 15) * 15 : raw
      const nextRotation = normalizeRotation(Math.round(snapped * 10) / 10)
      setBox(t.x * scale, t.y * scale, t.width * scale, t.height * scale, nextRotation)

      if (last) {
        rotateCenter.current = null
        onCommit({
          x: t.x,
          y: t.y,
          width: t.width,
          height: t.height,
          rotationDeg: nextRotation,
        })
      }
    },
    {
      pointer: { buttons: 1, touch: true },
      preventDefault: true,
      eventOptions: { passive: false },
    },
  )

  const handleEdgePointerDown = (edge: ResizeEdge) => {
    if (!interactive || !selected) return
    isResizeRef.current = true
    edgeRef.current = edge
  }

  const w = t.width * scale
  const h = t.height * scale

  return (
    <div
      ref={boxRef}
      {...bind()}
      style={{
        position: 'absolute',
        top: t.y * scale,
        left: t.x * scale,
        transform: `rotate(${t.rotationDeg ?? 0}deg)`,
        transformOrigin: 'center',
        width: w,
        height: h,
        pointerEvents: interactive ? 'auto' : 'none',
        zIndex: selected ? 20 : 10,
        touchAction: 'none',
        cursor: interactive ? 'move' : 'default',
      }}
    >
      <div
        className={`absolute inset-0 rounded-md ${
          selected
            ? 'border-[3px] border-primary bg-primary/15'
            : 'border-2 border-rose-400/60 bg-rose-400/5'
        }`}
      />
      <div
        className={`pointer-events-none absolute -top-1.5 -left-1.5 flex h-4 w-4 items-center justify-center rounded-full text-[9px] font-semibold text-white shadow ${
          selected ? 'bg-primary' : 'bg-rose-400'
        }`}
      >
        {index + 1}
      </div>
      {selected && interactive && (
        <>
          <ResizeHandles onEdgePointerDown={handleEdgePointerDown} />
          <RotateHandle bind={rotateBind} />
        </>
      )}
    </div>
  )
}

function BlockSprite({ node, scale }: { node: TextNodeEntry; scale: number }) {
  const sprite = (node.data.sprite as string | null | undefined) ?? undefined
  const { data: src } = useBlobImage(sprite)
  if (!src) return null
  const spriteT = node.data.spriteTransform
  const x = (spriteT?.x ?? node.transform.x) * scale
  const y = (spriteT?.y ?? node.transform.y) * scale
  const w = (spriteT?.width ?? node.transform.width) * scale
  const h = (spriteT?.height ?? node.transform.height) * scale
  return (
    <img
      alt=''
      src={src}
      draggable={false}
      className='pointer-events-none absolute select-none'
      style={{
        top: y,
        left: x,
        width: spriteT ? w : undefined,
        height: spriteT ? h : undefined,
        transformOrigin: 'center',
        transform: `rotate(${spriteT?.rotationDeg ?? node.transform.rotationDeg ?? 0}deg)${
          spriteT ? '' : ` scale(${scale})`
        }`,
      }}
    />
  )
}

function ResizeHandles({ onEdgePointerDown }: { onEdgePointerDown: (edge: ResizeEdge) => void }) {
  const s = RESIZE_HANDLE_SIZE
  const half = s / 2

  const edges: { edge: ResizeEdge; style: React.CSSProperties; cursor: string }[] = [
    {
      edge: { top: true, left: true, bottom: false, right: false },
      cursor: 'nwse-resize',
      style: { top: -half, left: -half, width: s, height: s },
    },
    {
      edge: { top: true, left: false, bottom: false, right: true },
      cursor: 'nesw-resize',
      style: { top: -half, right: -half, width: s, height: s },
    },
    {
      edge: { top: false, left: true, bottom: true, right: false },
      cursor: 'nesw-resize',
      style: { bottom: -half, left: -half, width: s, height: s },
    },
    {
      edge: { top: false, left: false, bottom: true, right: true },
      cursor: 'nwse-resize',
      style: { bottom: -half, right: -half, width: s, height: s },
    },
    {
      edge: { top: true, left: false, bottom: false, right: false },
      cursor: 'ns-resize',
      style: { top: -half, left: s, right: s, height: s },
    },
    {
      edge: { top: false, left: false, bottom: true, right: false },
      cursor: 'ns-resize',
      style: { bottom: -half, left: s, right: s, height: s },
    },
    {
      edge: { top: false, left: true, bottom: false, right: false },
      cursor: 'ew-resize',
      style: { left: -half, top: s, bottom: s, width: s },
    },
    {
      edge: { top: false, left: false, bottom: false, right: true },
      cursor: 'ew-resize',
      style: { right: -half, top: s, bottom: s, width: s },
    },
  ]

  return (
    <>
      {edges.map((e, i) => (
        <div
          key={i}
          onPointerDown={() => onEdgePointerDown(e.edge)}
          style={{ position: 'absolute', ...e.style, cursor: e.cursor, zIndex: 30 }}
        />
      ))}
    </>
  )
}

function RotateHandle({ bind }: { bind: ReturnType<typeof useDrag> }) {
  const s = ROTATE_HANDLE_SIZE
  const half = s / 2

  return (
    <>
      <div
        aria-hidden
        className='pointer-events-none absolute left-1/2 w-px -translate-x-1/2 bg-primary'
        style={{ top: '100%', height: ROTATE_HANDLE_DISTANCE }}
      />
      <div
        {...bind()}
        role='button'
        aria-label='Rotate text block'
        className='absolute left-1/2 z-40 rounded-full border-2 border-primary bg-background shadow-sm'
        style={{
          top: `calc(100% + ${ROTATE_HANDLE_DISTANCE}px)`,
          width: s,
          height: s,
          marginLeft: -half,
          marginTop: -half,
          cursor: 'grab',
          touchAction: 'none',
        }}
      />
    </>
  )
}

function getElementCenter(el: HTMLElement | null): Point | null {
  if (!el) return null
  const rect = el.getBoundingClientRect()
  return {
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2,
  }
}

function getPointer(event: Event | undefined): Point | null {
  if (!event || !('clientX' in event) || !('clientY' in event)) return null
  const pointer = event as PointerEvent
  return { x: pointer.clientX, y: pointer.clientY }
}

function angleFromCenter(center: Point, pointer: Point): number {
  const rad = Math.atan2(pointer.y - center.y, pointer.x - center.x)
  return (rad * 180) / Math.PI - 90
}

function normalizeRotation(degrees: number): number {
  let normalized = degrees % 360
  if (normalized > 180) normalized -= 360
  if (normalized <= -180) normalized += 360
  return normalized
}

function isShiftEvent(event: Event | undefined): boolean {
  return !!event && 'shiftKey' in event && !!(event as PointerEvent).shiftKey
}
