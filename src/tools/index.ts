import type { AgentTool } from "@mariozechner/pi-agent-core";
import type { SandboxContext } from "../sandbox.js";
import type { MemoryLayerDef } from "../plugin-types.js";
import { createCliTool } from "./cli.js";
import { createReadTool } from "./read.js";
import { createWriteTool } from "./write.js";
import { createEditTool } from "./edit.js";
import { createMemoryTool } from "./memory.js";
import { createTaskTrackerTool } from "./task-tracker.js";
import { createSkillLearnerTool } from "./skill-learner.js";
import { createCapturePhotoTool } from "./capture-photo.js";
import { createSandboxCliTool } from "./sandbox-cli.js";
import { createSandboxReadTool } from "./sandbox-read.js";
import { createSandboxWriteTool } from "./sandbox-write.js";
import { createSandboxEditTool } from "./sandbox-edit.js";
import { createSandboxMemoryTool } from "./sandbox-memory.js";

export interface BuiltinToolsConfig {
	dir: string;
	timeout?: number;
	sandbox?: SandboxContext;
	gitagentDir?: string;
	pluginMemoryLayers?: MemoryLayerDef[];
}

/**
 * Create the built-in tools (cli, read, write, memory, task_tracker, skill_learner).
 * If a SandboxContext is provided, returns sandbox-backed tools;
 * otherwise returns the standard local tools.
 */
// Built-in tools that write files or run shell/git. pi-agent-core parallelizes
// tool calls by default; these must run one-at-a-time or concurrent writes and
// git operations race (corrupt files, index.lock). read/grep stay parallel.
const SEQUENTIAL_TOOL_NAMES = new Set([
	"cli", "write", "edit", "memory", "capture_photo", "task_tracker", "skill_learner",
]);

function markSequential(tools: AgentTool<any>[]): AgentTool<any>[] {
	for (const t of tools) {
		if (SEQUENTIAL_TOOL_NAMES.has(t.name)) t.executionMode = "sequential";
	}
	return tools;
}

export function createBuiltinTools(config: BuiltinToolsConfig): AgentTool<any>[] {
	if (config.sandbox) {
		return markSequential([
			createSandboxCliTool(config.sandbox, config.timeout),
			createSandboxReadTool(config.sandbox),
			createSandboxWriteTool(config.sandbox),
			createSandboxEditTool(config.sandbox),
			createSandboxMemoryTool(config.sandbox),
		]);
	}

	const tools: AgentTool<any>[] = [
		createCliTool(config.dir, config.timeout),
		createReadTool(config.dir),
		createWriteTool(config.dir),
		createEditTool(config.dir),
		createMemoryTool(config.dir, config.pluginMemoryLayers),
		createCapturePhotoTool(config.dir),
	];

	// Add learning tools if gitagentDir is available
	if (config.gitagentDir) {
		tools.push(createTaskTrackerTool(config.dir, config.gitagentDir));
		tools.push(createSkillLearnerTool(config.dir, config.gitagentDir));
	}

	return markSequential(tools);
}
