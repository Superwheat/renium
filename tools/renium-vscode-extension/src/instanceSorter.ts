import { compareExplorerNodes } from "./serviceDefaults";
import type { ExplorerSortableNode } from "./serviceDefaults";

export type SortableNode = ExplorerSortableNode;

export class InstanceSorter {
    public sortNodes(nodes: SortableNode[]): SortableNode[] {
        return nodes.sort(compareExplorerNodes);
    }
}
