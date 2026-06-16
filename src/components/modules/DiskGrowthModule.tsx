// ============================================================================
// C 盘全盘变化分析模块
//
// 全盘变化只负责定位空间增减来源，不提供删除能力，避免把“变化目录”误当成“可清理目录”。
// ============================================================================

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { openUrl } from '@tauri-apps/plugin-opener';
import {
  AlertTriangle,
  CheckCircle2,
  FolderOpen,
  HardDrive,
  Loader2,
  Minus,
  Search,
  XCircle,
  Sparkles,
  TrendingDown,
  TrendingUp,
} from 'lucide-react';
import { ModuleCard } from '../ModuleCard';
import { EmptyState } from '../EmptyState';
import { useDashboard } from '../../contexts/DashboardContext';
import { useSettings } from '../../contexts';
import {
  checkAdminPrivilege,
  cancelDiskGrowthScan,
  openInFolder,
  scanDiskGrowth,
  type DiskGrowthAnalyzeEntry,
  type DiskGrowthEntry,
  type DiskGrowthReport,
  type DiskGrowthScanProgress,
  type DiskGrowthScanResponse,
} from '../../api/commands';
import { formatSize } from '../../utils/format';

function simplifyPath(path: string): string {
  const normalized = path.replace(/\\/g, '/');
  const parts = normalized.split('/').filter(Boolean);
  if (parts.length <= 3) return normalized;
  return `.../${parts.slice(-3).join('/')}`;
}

function formatDiff(diff: number): string {
  const sign = diff >= 0 ? '+' : '-';
  return `${sign}${formatSize(Math.abs(diff))}`;
}

function formatProgressCount(progress: DiskGrowthScanProgress | null): string {
  if (!progress) return '';
  const processed = progress.processed.toLocaleString();
  if (typeof progress.total === 'number' && progress.total > 0) {
    return `${processed} / ${progress.total.toLocaleString()}`;
  }
  return processed;
}

function getPhaseLabel(stage: string): string {
  switch (stage) {
    case 'mft':
      return '枚举 MFT';
    case 'path':
      return '重建路径';
    case 'metadata':
      return '读取大小';
    case 'aggregate':
      return '聚合目录';
    default:
      return '扫描中';
  }
}

function formatDuration(ms?: number): string {
  if (ms === undefined || ms < 0) return '-';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatPreviousScanTime(scanSummary: DiskGrowthScanResponse): string {
  return scanSummary.previous_scan_time || '暂无历史快照';
}

function getGrowthStyle(level: DiskGrowthEntry['level']) {
  switch (level) {
    case 'significant':
      return { icon: TrendingUp, color: 'text-red-500', label: '显著增长' };
    case 'fast':
      return { icon: TrendingUp, color: 'text-orange-500', label: '快速增长' };
    case 'minor':
      return { icon: TrendingUp, color: 'text-amber-500', label: '轻微增长' };
    case 'decreased':
      return { icon: TrendingDown, color: 'text-green-500', label: '已减少' };
    case 'new':
      return { icon: Sparkles, color: 'text-blue-500', label: '新增目录' };
    default:
      return { icon: Minus, color: 'text-[var(--text-faint)]', label: '稳定' };
  }
}

function SummaryCards({
  scanSummary,
  growthReport,
}: {
  scanSummary: DiskGrowthScanResponse;
  growthReport: DiskGrowthReport;
}) {
  const totalGrowth = growthReport.total_growth;

  return (
    <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
      <div className="bg-[var(--bg-main)] rounded-xl px-4 py-3">
        <p className="text-[11px] text-[var(--text-muted)] mb-1">C 盘已索引占用</p>
        <p className="text-lg font-bold text-[var(--text-primary)] tabular-nums">
          {formatSize(scanSummary.total_size)}
        </p>
      </div>
      <div className="bg-[var(--bg-main)] rounded-xl px-4 py-3">
        <p className="text-[11px] text-[var(--text-muted)] mb-1">与上次净变化</p>
        <p
          className={`text-lg font-bold tabular-nums ${
            totalGrowth > 0
              ? 'text-red-500'
              : totalGrowth < 0
                ? 'text-green-500'
                : 'text-[var(--text-muted)]'
          }`}
        >
          {totalGrowth === 0 ? '暂无变化' : formatDiff(totalGrowth)}
        </p>
      </div>
      <div className="bg-[var(--bg-main)] rounded-xl px-4 py-3">
        <p className="text-[11px] text-[var(--text-muted)] mb-1">上次扫描</p>
        <p className="text-sm font-semibold text-[var(--text-primary)] tabular-nums truncate">
          {formatPreviousScanTime(scanSummary)}
        </p>
      </div>
      <div className="bg-[var(--bg-main)] rounded-xl px-4 py-3">
        <p className="text-[11px] text-[var(--text-muted)] mb-1">扫描文件数</p>
        <p className="text-lg font-bold text-[var(--brand-green)] tabular-nums">
          {scanSummary.total_files_scanned.toLocaleString()}
        </p>
      </div>
      {growthReport.entries.length > 0 && (
        <div className="col-span-4 flex items-center gap-3 text-[12px] text-[var(--text-muted)]">
          <span className="text-red-500">新增 {formatSize(scanSummary.analyze.increased_size ?? 0)}</span>
          <span className="text-green-500">减少 {formatSize(scanSummary.analyze.decreased_size ?? 0)}</span>
          <span>按变化量排序</span>
        </div>
      )}
    </div>
  );
}

function DiagnosticBanner({ report }: { report: DiskGrowthReport }) {
  const hasGrowth = report.significant_count > 0 || report.fast_count > 0;

  return (
    <div
      className={`flex items-start gap-3 px-4 py-3 rounded-xl text-[13px] ${
        hasGrowth
          ? 'bg-amber-500/10 text-amber-700 dark:text-amber-400'
          : 'bg-[var(--brand-green-10)] text-[var(--brand-green)]'
      }`}
    >
      {hasGrowth ? (
        <AlertTriangle className="w-4 h-4 mt-0.5 shrink-0" />
      ) : (
        <CheckCircle2 className="w-4 h-4 mt-0.5 shrink-0" />
      )}
      <span>{report.summary}</span>
    </div>
  );
}

function DiskGrowthDiagnostics({
  scanSummary,
  resultMode,
  maxEntries,
}: {
  scanSummary: DiskGrowthScanResponse;
  resultMode: 'change' | 'usage';
  maxEntries: number;
}) {
  // 与大目录分析复用同一类诊断布局，让用户在不同 MFT 模块里看到一致的阶段耗时信息。
  const hasPhaseDurations = scanSummary.phase_durations.length > 0;
  const latestPhase = hasPhaseDurations
    ? scanSummary.phase_durations[scanSummary.phase_durations.length - 1]
    : null;

  return (
    <div className="rounded-xl bg-[var(--bg-main)] border border-[var(--border-color)] px-4 py-3 text-xs space-y-3">
      <div className="grid grid-cols-1 md:grid-cols-[1fr_auto] gap-2 md:gap-4">
        <div className="flex items-center gap-2 min-w-0">
          <span className="shrink-0 px-2 py-0.5 rounded-md bg-[var(--brand-green)] text-white font-medium">
            {latestPhase ? getPhaseLabel(latestPhase.stage) : '扫描完成'}
          </span>
          <span className="truncate text-[var(--text-primary)]">
            {resultMode === 'change'
              ? `展示变化目录，按变化量排序，最多 ${maxEntries} 项`
              : '暂无变化目录，展示本次占用基线'}
          </span>
        </div>
        <div className="flex items-center justify-start md:justify-end gap-4 text-[var(--text-muted)] tabular-nums">
          <span>已处理 {scanSummary.total_files_scanned.toLocaleString()}</span>
          <span>总耗时 {formatDuration(scanSummary.scan_duration_ms)}</span>
        </div>
      </div>

      {hasPhaseDurations && (
        <div className="grid grid-cols-2 md:grid-cols-4 gap-2">
          {scanSummary.phase_durations.map((phase, index) => (
            <div
              key={`${phase.stage}-${index}`}
              className="min-w-0 rounded-lg bg-[var(--bg-card)] border border-[var(--border-color)] px-2.5 py-2"
            >
              <div className="truncate text-[var(--text-muted)]">{getPhaseLabel(phase.stage)}</div>
              <div className="mt-0.5 text-[var(--text-primary)] font-semibold tabular-nums">
                {formatDuration(phase.duration_ms)}
              </div>
            </div>
          ))}
        </div>
      )}

      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-1 text-[var(--text-faint)]">
        <span>引擎：{scanSummary.backend === 'mft' ? 'MFT' : scanSummary.backend}</span>
        <span>
          大小来源：MFT {scanSummary.mft_size_count.toLocaleString()} 个，metadata 回退{' '}
          {scanSummary.metadata_fallback_count.toLocaleString()} 个
        </span>
      </div>
    </div>
  );
}

function entryFromGrowth(growth: DiskGrowthEntry): DiskGrowthAnalyzeEntry {
  return {
    path: growth.path,
    size: growth.new_size,
    category: '变化目录',
    risk: 'safe',
    action: 'ignore',
    reason: growth.explanation,
    suggestion: growth.suggestion,
    matched_rule_id: null,
    tags: [],
  };
}

function ChangeRow({
  entry,
  growth,
  onOpenFolder,
  onSearchPath,
}: {
  entry: DiskGrowthAnalyzeEntry;
  growth: DiskGrowthEntry | null;
  onOpenFolder: (path: string) => void;
  onSearchPath: (path: string) => void;
}) {
  const style = growth ? getGrowthStyle(growth.level) : getGrowthStyle('stable');
  const Icon = style.icon;
  const diff = growth?.diff ?? 0;

  return (
    <div className="flex items-center gap-3 px-4 py-2.5 hover:bg-[var(--bg-hover)] transition-colors group">
      <div
        className={`w-1.5 h-8 rounded-full shrink-0 ${
          diff > 0 ? 'bg-red-400' : diff < 0 ? 'bg-green-400' : 'bg-gray-300'
        }`}
      />

      <div className="flex-1 min-w-0">
        <p
          className="text-[13px] text-[var(--text-primary)] truncate cursor-pointer hover:text-[var(--brand-green)] transition-colors"
          title={entry.path}
          onClick={() => onOpenFolder(entry.path)}
        >
          {simplifyPath(entry.path)}
        </p>
        <p className="text-[11px] text-[var(--text-faint)] mt-0.5 truncate">
          {growth?.explanation ?? entry.reason}
        </p>
      </div>

      <span className="px-2 py-0.5 rounded text-[11px] bg-[var(--bg-hover)] text-[var(--text-muted)] shrink-0">
        {entry.category}
      </span>

      <span className={`flex items-center gap-1 w-24 justify-end text-[12px] shrink-0 ${style.color}`}>
        <Icon className="w-3.5 h-3.5" />
        {style.label}
      </span>

      <span className="text-[13px] font-medium text-[var(--text-primary)] tabular-nums w-20 text-right shrink-0">
        {formatSize(entry.size)}
      </span>

      <span className={`text-[13px] font-medium tabular-nums w-24 text-right shrink-0 ${style.color}`}>
        {diff === 0 ? '-' : formatDiff(diff)}
      </span>

      <div className="flex w-16 shrink-0 justify-end gap-0.5 opacity-0 transition group-hover:opacity-100">
        <button
          onClick={() => onSearchPath(entry.path)}
          className="p-1.5 rounded-lg text-[var(--text-muted)] hover:bg-[var(--brand-green-10)] hover:text-[var(--brand-green)] transition"
          title="搜索该路径有什么作用"
        >
          <Search className="w-4 h-4" />
        </button>
        <button
          onClick={() => onOpenFolder(entry.path)}
          className="p-1.5 rounded-lg text-[var(--text-muted)] hover:bg-[var(--brand-green-10)] hover:text-[var(--brand-green)] transition"
          title="打开目录"
        >
          <FolderOpen className="w-4 h-4" />
        </button>
      </div>
    </div>
  );
}

export function DiskGrowthModule() {
  const { modules, expandedModule, setExpandedModule, updateModuleState, oneClickScanTrigger, stopScanTrigger } = useDashboard();
  const { settings } = useSettings();
  const moduleState = modules.diskGrowth;
  const lastScanTriggerRef = useRef(0);
  const scanningRef = useRef(false);
  const cancelRequestedRef = useRef(false);
  const scanRunIdRef = useRef(0);

  const [scanSummary, setScanSummary] = useState<DiskGrowthScanResponse | null>(null);
  const [growthReport, setGrowthReport] = useState<DiskGrowthReport | null>(null);
  const [isAdmin, setIsAdmin] = useState<boolean | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const [scanElapsed, setScanElapsed] = useState(0);
  const [scanProgress, setScanProgress] = useState<DiskGrowthScanProgress | null>(null);

  const isExpanded = expandedModule === 'disk-growth';

  useEffect(() => {
    let cancelled = false;
    checkAdminPrivilege()
      .then((result) => {
        if (!cancelled) setIsAdmin(result);
      })
      .catch(() => {
        if (!cancelled) setIsAdmin(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (moduleState.status !== 'scanning') {
      setScanElapsed(0);
      return;
    }

    const startedAt = Date.now();
    const timer = window.setInterval(() => {
      setScanElapsed(Math.floor((Date.now() - startedAt) / 1000));
    }, 1000);

    return () => window.clearInterval(timer);
  }, [moduleState.status]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    listen<DiskGrowthScanProgress>('disk-growth:progress', (event) => {
      if (!cancelled) setScanProgress(event.payload);
    }).then((handler) => {
      if (cancelled) {
        handler();
      } else {
        unlisten = handler;
      }
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const handleScan = useCallback(async () => {
    if (scanningRef.current) return;
    if (isAdmin === false) {
      const message = 'C 盘全盘分析需要管理员权限读取 MFT，请以管理员身份启动应用后再扫描。';
      setError(message);
      updateModuleState('diskGrowth', { status: 'error', error: message });
      return;
    }
    scanningRef.current = true;
    cancelRequestedRef.current = false;
    const scanRunId = ++scanRunIdRef.current;

    updateModuleState('diskGrowth', { status: 'scanning', error: null, fileCount: 0, totalSize: 0 });
    setError(null);
    setScanSummary(null);
    setGrowthReport(null);
    setScanProgress(null);
    setShowAll(false);

    try {
      const result = await scanDiskGrowth(settings.diskGrowthMaxEntries);
      if (cancelRequestedRef.current || scanRunId !== scanRunIdRef.current) {
        // 用户取消后不接收可能已经返回的旧结果，避免把被中断的扫描写成正常完成。
        return;
      }
      setScanSummary(result);
      setGrowthReport(result.growth);
      updateModuleState('diskGrowth', {
        status: 'done',
        fileCount: result.growth.entries.length,
        totalSize: Math.abs(result.growth.total_growth),
      });
    } catch (err) {
      if (cancelRequestedRef.current || scanRunId !== scanRunIdRef.current) {
        updateModuleState('diskGrowth', { status: 'idle', progress: 0 });
        return;
      }
      const message = String(err);
      setError(message);
      updateModuleState('diskGrowth', { status: 'error', error: message });
    } finally {
      if (scanRunId === scanRunIdRef.current) {
        scanningRef.current = false;
      }
    }
  }, [isAdmin, settings.diskGrowthMaxEntries, updateModuleState]);

  const handleStopScan = useCallback(async () => {
    cancelRequestedRef.current = true;
    scanRunIdRef.current += 1;
    scanningRef.current = false;
    updateModuleState('diskGrowth', { status: 'idle', progress: 0 });
    setScanProgress(null);
    try {
      await cancelDiskGrowthScan();
    } catch (err) {
      console.error('停止 C 盘全盘分析失败:', err);
    }
  }, [updateModuleState]);

  useEffect(() => {
    if (oneClickScanTrigger > 0 && oneClickScanTrigger !== lastScanTriggerRef.current) {
      lastScanTriggerRef.current = oneClickScanTrigger;
      handleScan();
    }
  }, [oneClickScanTrigger, handleScan]);

  useEffect(() => {
    if (stopScanTrigger > 0 && scanningRef.current) {
      // 全局停止按钮已发后端取消信号；本地只标记取消，避免旧扫描结果回写 UI。
      cancelRequestedRef.current = true;
      scanRunIdRef.current += 1;
      scanningRef.current = false;
      setScanProgress(null);
    }
  }, [stopScanTrigger]);

  const handleOpenFolder = useCallback(async (path: string) => {
    try {
      await openInFolder(path);
    } catch (err) {
      console.error('打开目录失败:', err);
    }
  }, []);

  const handleSearchPath = useCallback(async (path: string) => {
    try {
      // 变化目录不等于可清理目录，搜索文案先确认用途，再辅助判断是否可删。
      const query = encodeURIComponent(`Windows 路径 ${path} 是什么 有什么作用 可以删除吗`);
      await openUrl(`https://www.bing.com/search?q=${query}`);
    } catch (err) {
      console.error('搜索路径用途失败:', err);
    }
  }, []);

  const growthMap = useMemo(() => {
    const map = new Map<string, DiskGrowthEntry>();
    for (const entry of growthReport?.entries ?? []) {
      map.set(entry.path.toLowerCase().replace(/\\/g, '/'), entry);
    }
    return map;
  }, [growthReport]);

  const entries = growthReport?.entries.length
    ? growthReport.entries.map(entryFromGrowth)
    : scanSummary?.analyze.entries ?? [];
  const resultMode = growthReport?.entries.length ? 'change' : 'usage';
  const displayedEntries = showAll ? entries : entries.slice(0, 20);
  const hasMore = entries.length > displayedEntries.length;

  return (
    <ModuleCard
      id="disk-growth"
      title="C 盘全盘分析"
      description="基于 MFT 快速扫描 C 盘，对比上次快照定位空间变化来源"
      icon={<HardDrive className="w-5 h-5 text-[var(--brand-green)]" />}
      status={moduleState.status}
      fileCount={moduleState.fileCount}
      totalSize={moduleState.totalSize}
      countLabel="个变化目录"
      expanded={isExpanded}
      onToggleExpand={() => setExpandedModule(isExpanded ? null : 'disk-growth')}
      onScan={handleScan}
      scanButtonText="开始扫描"
      scanDisabled={isAdmin === false}
      error={error}
    >
      {isAdmin === false && (
        <div className="mx-4 mt-4 flex items-start gap-3 rounded-xl bg-amber-500/10 px-4 py-3 text-[13px] text-amber-700 dark:text-amber-400">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
      <span>当前未检测到管理员权限。MFT 全盘分析需要管理员权限，请以管理员身份启动 LightC 或运行开发命令。</span>
        </div>
      )}

      {moduleState.status === 'idle' && !scanSummary && !growthReport && (
        <div className="p-4">
          <EmptyState
            icon={HardDrive}
            title="尚未扫描 C 盘变化"
            description="点击开始扫描，建立 C 盘快照；再次扫描后会对比新增、减少和明显变化的目录。"
          />
        </div>
      )}

      {moduleState.status === 'scanning' && (
        <div className="flex flex-col items-center justify-center py-12 text-[var(--text-muted)]">
          <Loader2 className="w-8 h-8 animate-spin text-[var(--brand-green)] mb-3" />
          <p className="text-sm">{scanProgress?.message ?? '正在通过 MFT 扫描 C 盘...'}</p>
          <p className="text-xs text-[var(--text-muted)] mt-1 tabular-nums">
            已用时 {scanElapsed}s
          </p>
          {scanProgress && (
            <p className="text-xs text-[var(--text-faint)] mt-1 tabular-nums">
              {getPhaseLabel(scanProgress.stage)} {formatProgressCount(scanProgress)}
            </p>
          )}
          <p className="text-xs text-[var(--text-faint)] mt-1">
            首次扫描会建立快照，下次扫描开始展示新增和减少的目录
          </p>
          <button
            onClick={handleStopScan}
            className="mt-4 flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium
              bg-red-50 dark:bg-red-900/20 text-red-500 hover:bg-red-100 dark:hover:bg-red-900/30
              border border-red-200 dark:border-red-800/30 transition-colors"
          >
            <XCircle className="w-3.5 h-3.5" />
            停止扫描
          </button>
        </div>
      )}

      {moduleState.status === 'done' && scanSummary && growthReport && (
        <div className="p-4 space-y-4">
          <SummaryCards scanSummary={scanSummary} growthReport={growthReport} />
          <DiagnosticBanner report={growthReport} />

          <div className="flex items-center justify-between">
            <p className="text-[13px] text-[var(--text-muted)]">共 {entries.length} 个目录结果</p>
            <span className="text-[12px] text-[var(--text-faint)]">{isAdmin ? '管理员模式' : '非管理员模式'}</span>
          </div>
          <DiskGrowthDiagnostics
            scanSummary={scanSummary}
            resultMode={resultMode}
            maxEntries={settings.diskGrowthMaxEntries}
          />

          <div className="bg-[var(--bg-main)] rounded-xl overflow-hidden">
            <div className="flex items-center gap-3 px-4 py-2 border-b border-[var(--border-color)] text-[11px] text-[var(--text-faint)] uppercase tracking-wider">
              <div className="w-1.5 shrink-0" />
              <div className="flex-1">路径</div>
              <div className="w-16 shrink-0 text-center">分类</div>
              <div className="w-24 shrink-0 text-right">变化级别</div>
              <div className="w-20 shrink-0 text-right">当前大小</div>
              <div className="w-24 shrink-0 text-right">变化量</div>
              <div className="w-16 shrink-0" />
            </div>

            {displayedEntries.map((entry) => {
              const normalizedPath = entry.path.toLowerCase().replace(/\\/g, '/');
              return (
                <ChangeRow
                  key={entry.path}
                  entry={entry}
                  growth={growthMap.get(normalizedPath) ?? null}
                  onOpenFolder={handleOpenFolder}
                  onSearchPath={handleSearchPath}
                />
              );
            })}

            {hasMore && (
              <button
                onClick={() => setShowAll(true)}
                className="w-full py-3 text-center text-[13px] text-[var(--brand-green)] hover:bg-[var(--bg-hover)] transition-colors"
              >
                显示全部 {entries.length} 项
              </button>
            )}

            {entries.length === 0 && (
              <div className="p-4">
                <EmptyState
                  icon={HardDrive}
                  title="已建立首次快照"
                  description="下次扫描后会对比本次快照，并在这里显示新增、减少或明显变化的目录。"
                  tone="success"
                  compact
                />
              </div>
            )}
          </div>
        </div>
      )}
    </ModuleCard>
  );
}

export default DiskGrowthModule;
