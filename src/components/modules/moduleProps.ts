import type { LayoutMode } from '../../config/moduleMeta';

export interface ModuleRenderProps {
  layoutMode?: LayoutMode;
  /**
   * 页面模式下只有当前模块需要渲染完整 DOM；非当前模块仍保持 React 实例和本地 state，
   * 但跳过大列表/图表渲染，降低菜单切换时的协调和布局成本。
   */
  isPageActive?: boolean;
}

export function shouldSkipInactivePageRender(layoutMode: LayoutMode = 'cards', isPageActive = true) {
  return layoutMode === 'pages' && !isPageActive;
}
