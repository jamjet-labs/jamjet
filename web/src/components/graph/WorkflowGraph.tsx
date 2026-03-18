import { useEffect, useMemo } from 'react'
import {
  ReactFlow,
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  useNodesState,
  useEdgesState,
  type Node,
  type Edge,
  type NodeTypes,
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'
import dagre from 'dagre'

import { useInspectorStore } from '@/store/inspector'
import { useExecution, useEvents } from '@/hooks/useExecution'
import type { WorkflowIr, WorkflowIrNode, WorkflowIrEdge, Event } from '@/api/types'
import NodeRenderer, { type NodeData } from './NodeRenderer'

// ─── Constants ────────────────────────────────────────────────────────────────

const NODE_WIDTH = 160
const NODE_HEIGHT = 72

// React Flow requires the custom node map to be typed as NodeTypes
const nodeTypes: NodeTypes = { custom: NodeRenderer }

// ─── Mock IR (used when the API execution has no workflow_ir field) ────────────

const MOCK_IR: WorkflowIr = {
  workflow_id: 'mock',
  start_node: 'start',
  nodes: [
    { id: 'start', kind: 'start' },
    { id: 'model_1', kind: 'model' },
    { id: 'tool_1', kind: 'tool' },
    { id: 'coordinator_1', kind: 'coordinator' },
    { id: 'end', kind: 'end' },
  ],
  edges: [
    { from: 'start', to: 'model_1' },
    { from: 'model_1', to: 'tool_1' },
    { from: 'model_1', to: 'coordinator_1' },
    { from: 'tool_1', to: 'end' },
    { from: 'coordinator_1', to: 'end' },
  ],
}

// ─── Dagre layout ─────────────────────────────────────────────────────────────

type FlowNode = Node<NodeData>

function applyDagreLayout(rfNodes: FlowNode[], rfEdges: Edge[]): FlowNode[] {
  const g = new dagre.graphlib.Graph()
  g.setDefaultEdgeLabel(() => ({}))
  g.setGraph({ rankdir: 'TB', nodesep: 60, ranksep: 70 })

  rfNodes.forEach((n) => g.setNode(n.id, { width: NODE_WIDTH, height: NODE_HEIGHT }))
  rfEdges.forEach((e) => g.setEdge(e.source, e.target))

  dagre.layout(g)

  return rfNodes.map((n) => {
    const pos = g.node(n.id)
    return {
      ...n,
      position: {
        x: pos.x - NODE_WIDTH / 2,
        y: pos.y - NODE_HEIGHT / 2,
      },
    }
  })
}

// ─── Status from events ───────────────────────────────────────────────────────

type NodeStatus = 'pending' | 'scheduled' | 'running' | 'completed' | 'failed' | 'skipped'

// Helper: the EventKind union includes UnknownEvent with `[key:string]:unknown`,
// which prevents TypeScript from narrowing `node_id` to string after a type check.
// We extract node_id explicitly and assert it.
function nodeIdOf(kind: { type: string; [key: string]: unknown }): string {
  return kind['node_id'] as string
}

function computeNodeStatuses(events: Event[]): Record<string, NodeStatus> {
  const statuses: Record<string, NodeStatus> = {}

  for (const event of events) {
    const kind = event.kind as { type: string; [key: string]: unknown }
    if (kind.type === 'node_completed') {
      statuses[nodeIdOf(kind)] = 'completed'
    } else if (kind.type === 'node_failed') {
      statuses[nodeIdOf(kind)] = 'failed'
    } else if (kind.type === 'node_started') {
      const id = nodeIdOf(kind)
      const current = statuses[id]
      if (current !== 'completed' && current !== 'failed') {
        statuses[id] = 'running'
      }
    } else if (kind.type === 'node_scheduled') {
      const id = nodeIdOf(kind)
      if (!statuses[id]) {
        statuses[id] = 'scheduled'
      }
    }
  }

  return statuses
}

// ─── IR → React Flow conversion ───────────────────────────────────────────────

function irToFlow(
  ir: WorkflowIr,
  statuses: Record<string, NodeStatus>,
  selectedNodeId: string | null,
): { nodes: FlowNode[]; edges: Edge[] } {
  const rfNodes: FlowNode[] = ir.nodes.map((n: WorkflowIrNode) => ({
    id: n.id,
    type: 'custom',
    position: { x: 0, y: 0 }, // dagre will set real positions
    data: {
      label: n.label != null ? String(n.label) : n.id,
      nodeType: n.kind,
      status: statuses[n.id] ?? 'pending',
      selected: n.id === selectedNodeId,
    },
  }))

  const rfEdges: Edge[] = ir.edges.map((e: WorkflowIrEdge, idx: number) => ({
    id: `e-${String(e.from)}-${String(e.to)}-${idx}`,
    source: String(e.from),
    target: String(e.to),
    type: 'smoothstep',
    style: { stroke: '#71717a', strokeWidth: 1.5 },
    animated: statuses[String(e.from)] === 'running',
  }))

  const laidOut = applyDagreLayout(rfNodes, rfEdges)
  return { nodes: laidOut, edges: rfEdges }
}

// ─── Main component ───────────────────────────────────────────────────────────

export function WorkflowGraph() {
  const { selectedExecutionId, selectedNodeId, setNode } = useInspectorStore()

  const { data: execution } = useExecution(selectedExecutionId)
  const { data: events } = useEvents(selectedExecutionId)

  // Compute statuses from events
  const statuses = useMemo<Record<string, NodeStatus>>(
    () => computeNodeStatuses(events ?? []),
    [events],
  )

  // Derive the IR — use workflow_ir from execution if present, else MOCK_IR
  const ir = useMemo<WorkflowIr | null>(() => {
    if (!selectedExecutionId) return null
    // Server may extend the Execution payload with workflow_ir
    const ext = execution as (typeof execution & { workflow_ir?: WorkflowIr }) | undefined
    return ext?.workflow_ir ?? MOCK_IR
  }, [selectedExecutionId, execution])

  // Build React Flow nodes/edges
  const { nodes: initialNodes, edges: initialEdges } = useMemo(() => {
    if (!ir) return { nodes: [] as FlowNode[], edges: [] as Edge[] }
    return irToFlow(ir, statuses, selectedNodeId)
  }, [ir, statuses, selectedNodeId])

  const [nodes, setNodes, onNodesChange] = useNodesState<FlowNode>(initialNodes)
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges)

  // Sync whenever derived nodes/edges change
  useEffect(() => {
    setNodes(initialNodes)
  }, [initialNodes, setNodes])

  useEffect(() => {
    setEdges(initialEdges)
  }, [initialEdges, setEdges])

  // ── No execution selected ────────────────────────────────────────────────
  if (!selectedExecutionId) {
    return (
      <div className="flex items-center justify-center h-full text-zinc-500 text-sm select-none">
        Select an execution to view the workflow graph
      </div>
    )
  }

  return (
    <ReactFlow
      nodes={nodes}
      edges={edges}
      onNodesChange={onNodesChange}
      onEdgesChange={onEdgesChange}
      nodeTypes={nodeTypes}
      onNodeClick={(_event, node) => setNode(node.id)}
      onPaneClick={() => setNode(null)}
      fitView
      fitViewOptions={{ padding: 0.2 }}
      minZoom={0.2}
      maxZoom={2}
      colorMode="dark"
    >
      <Background
        variant={BackgroundVariant.Dots}
        gap={20}
        size={1}
        color="#3f3f46"
      />
      <MiniMap
        nodeColor={(n) => {
          const status = (n.data as NodeData).status
          switch (status) {
            case 'completed': return '#059669'
            case 'running':   return '#3b82f6'
            case 'failed':    return '#dc2626'
            case 'scheduled': return '#71717a'
            default:          return '#52525b'
          }
        }}
        maskColor="rgba(9,9,11,0.7)"
        style={{ background: '#18181b', border: '1px solid #3f3f46' }}
      />
      <Controls showInteractive={false} />
    </ReactFlow>
  )
}
