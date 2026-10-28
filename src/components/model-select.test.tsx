import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import { ModelSelect, QuickModelSelect, modelLabel, modelOptions, type ModelCatalog } from "./model-select";

const catalog: ModelCatalog = { models: [{ id: "gateway/coder", name: "编码模型" }], loading: false, error: "", refresh: vi.fn() };
describe("模型下拉选择", () => {
  it("显示真实列表，已有自定义值未在列表中时保留，不擅自替换", () => {
    expect(modelOptions(catalog.models, "custom/model").map((item) => item.id)).toEqual(["custom/model", "gateway/coder"]);
    expect(modelOptions(catalog.models, "gateway/coder")).toHaveLength(1);
    const html = renderToStaticMarkup(<ModelSelect label="主智能体模型" value="custom/model" onChange={vi.fn()} catalog={catalog} />);
    expect(html).toContain('role="combobox"');
    expect(html).toContain('aria-label="主智能体模型"');
    expect(html).toContain("custom/model · 当前设置");
    expect(html).toContain("1 个可用模型");
    expect(modelOptions(catalog.models, "custom/model")[1].name).toBe("编码模型");
  });
  it("执行角色提供继承选项，获取失败仍保留当前模型及手动入口", () => {
    const html = renderToStaticMarkup(<ModelSelect label="执行模型" value="" onChange={vi.fn()} emptyLabel="继承主智能体模型" catalog={{ ...catalog, models: [], error: "模型列表获取失败" }} />);
    expect(html).toContain("继承主智能体模型");
    expect(html).toContain("模型列表获取失败");
    expect(html).toContain('aria-label="刷新执行模型列表"');
  });
  it("对话输入区显示当前模型并说明选择用于后续请求", () => {
    const html = renderToStaticMarkup(<QuickModelSelect value="gateway/coder" catalog={catalog} disabled={false} onChange={vi.fn()} onSettings={vi.fn()} />);
    expect(html).toContain('aria-label="当前主智能体模型"');
    expect(html).toContain('role="combobox"');
    expect(html).toContain("编码模型");
    expect(html).toContain("选择主智能体模型；新设置用于后续请求");
  });
  it("保存期间禁用模型控件，加载列表不替换已选模型", () => {
    const html = renderToStaticMarkup(<ModelSelect label="角色预设模型" value="gateway/coder" disabled onChange={vi.fn()} catalog={{ ...catalog, loading: true }} />);
    expect(html).toContain('data-disabled=""');
    expect(html).toContain("编码模型");
    expect(html).toContain("正在获取可用模型");
  });
  it("疑似误填的密钥不作为模型标签直接展示", () => {
    const key = "sk-" + "a".repeat(40);
    expect(modelLabel(key)).not.toContain(key);
    expect(modelOptions([], key)[0].name).not.toContain(key);
  });
});
