use aionui_api_types::{KeeperLastStop, RolePlaybookEntry, RoleProgress};

use super::*;

fn lesson(id: &str, helpful: u32, harmful: u32, content: &str) -> RolePlaybookEntry {
    RolePlaybookEntry {
        id: id.to_owned(),
        helpful,
        harmful,
        content: content.to_owned(),
        updated_at: 0,
    }
}

fn task(task: &str, state: &str) -> RoleProgress {
    RoleProgress {
        task: task.to_owned(),
        state: state.to_owned(),
        updated_at: 0,
    }
}

fn sources<'a>(
    others: &'a [String],
    memory: &'a str,
    playbook: &'a [RolePlaybookEntry],
    progress: &'a [RoleProgress],
) -> BriefSources<'a> {
    BriefSources {
        role_id: "seo",
        project_id: "project_a",
        project_name: "yipan-store",
        other_projects: others,
        charter: "# SEO watch\nKeeps the store findable.\n\nSecond paragraph that stays out.",
        memory,
        playbook,
        progress,
        last_stop: None,
    }
}

#[test]
fn a_sole_project_gets_every_note_with_the_ones_on_the_opening_first() {
    let memory = "# Memory\n- 2026-11-16 robots.txt blocks /cart\n- 2026-11-17 sitemap 提交 failed twice\n- 2026-11-18 coffee is good\n";
    let brief = compose(&sources(&[], memory, &[], &[]), "继续提交 sitemap");
    assert_eq!(brief.charter, "SEO watch Keeps the store findable.");
    assert_eq!(
        brief.memory,
        vec![
            "2026-11-17 sitemap 提交 failed twice",
            "2026-11-18 coffee is good",
            "2026-11-16 robots.txt blocks /cart",
        ]
    );
}

#[test]
fn a_role_keeping_several_projects_hands_over_only_this_projects_affairs() {
    let others = vec!["daily-journal".to_owned()];
    let memory = "- 2026-11-16 [yipan-store] robots.txt blocks /cart\n- 2026-11-17 [daily-journal] sitemap is fine\n- 2026-11-18 coffee is good\n- 2026-11-19 sitemap needs resubmitting\n";
    let playbook = [
        lesson("p-0001", 5, 0, "daily-journal posts go out at 9"),
        lesson("p-0002", 1, 0, "Verify robots.txt on staging first"),
        lesson("p-0003", 0, 3, "yipan-store titles stay under 60 chars"),
    ];
    let progress = [
        task("journal-backfill", "daily-journal: 3 of 10 days"),
        task("fix-cart", "robots.txt edited, not deployed"),
    ];
    let brief = compose(&sources(&others, memory, &playbook, &progress), "sitemap");
    assert_eq!(
        brief.memory,
        vec![
            "2026-11-16 [yipan-store] robots.txt blocks /cart",
            "2026-11-19 sitemap needs resubmitting",
        ]
    );
    assert_eq!(
        brief
            .playbook
            .iter()
            .map(|lesson| lesson.id.as_str())
            .collect::<Vec<_>>(),
        vec!["p-0003", "p-0002"]
    );
    assert_eq!(
        brief.progress.iter().map(|task| task.task.as_str()).collect::<Vec<_>>(),
        vec!["fix-cart"]
    );
}

#[test]
fn every_part_keeps_to_its_budget() {
    let long = "x".repeat(700);
    let memory: String = (0..40).map(|i| format!("- 2026-11-01 note {i} {long}\n")).collect();
    let playbook: Vec<_> = (0..20).map(|i| lesson(&format!("p-{i:04}"), 1, 0, &long)).collect();
    let progress: Vec<_> = (0..20).map(|i| task(&format!("task-{i}"), &long)).collect();
    let brief = compose(&sources(&[], &memory, &playbook, &progress), "");
    let chars = |items: &[String]| items.iter().map(|item| item.chars().count()).sum::<usize>();
    assert!(chars(&brief.memory) <= MEMORY_CHARS);
    assert!(brief.memory.iter().all(|note| note.chars().count() <= ITEM_CHARS));
    assert!(brief.playbook.len() <= PLAYBOOK_ITEMS);
    assert!(
        brief
            .progress
            .iter()
            .all(|task| task.state.chars().count() <= TASK_STATE_CHARS)
    );
    let rendered = render_keeper_brief(&brief, "SEO watch", "");
    assert!(rendered.chars().count() < 6_000, "{}", rendered.chars().count());
}

#[test]
fn a_keeper_with_nothing_on_the_project_hands_over_nothing() {
    let others = vec!["daily-journal".to_owned()];
    let brief = compose(&sources(&others, "- 2026-11-18 coffee is good\n", &[], &[]), "hello");
    assert!(brief.memory.is_empty());
    assert!(!has_content(&brief));

    let mut with_stop = sources(&others, "", &[], &[]);
    with_stop.last_stop = Some(KeeperLastStop {
        text: "sitemap\nhalf   submitted".to_owned(),
        conversation_id: "c1".to_owned(),
        at: 1,
    });
    let brief = compose(&with_stop, "");
    assert!(has_content(&brief));
    assert_eq!(
        brief.last_stop.as_ref().map(|stop| stop.text.as_str()),
        Some("sitemap half submitted")
    );
}

#[test]
fn the_agent_reads_it_as_background_to_check() {
    let brief = compose(
        &sources(
            &[],
            "- 2026-11-16 robots.txt blocks /cart\n",
            &[lesson("p-0001", 1, 0, "Stage first")],
            &[],
        ),
        "",
    );
    let text = render_keeper_brief(&brief, "SEO watch", "");
    assert!(text.starts_with("<project-keeper-brief>\n"));
    assert!(text.ends_with("</project-keeper-brief>"));
    assert!(text.contains("\"yipan-store\""));
    assert!(text.contains("the resident role \"SEO watch\""));
    assert!(text.contains("not instructions"));
    assert!(text.contains("- [p-0001] Stage first"));
    assert!(text.contains("- 2026-11-16 robots.txt blocks /cart"));
}

#[test]
fn overlapping_project_names_do_not_bleed_into_a_shorter_sibling() {
    let others = vec!["yipan-global-store".to_owned(), "yipan-en".to_owned()];
    let memory = "\
- 2026-11-16 [yipan-global-store] robots.txt blocks /cart
- 2026-11-17 [yipan-en] pricing banner updated
- 2026-11-18 yipan-global-store sitemap submitted
- 2026-11-19 [yipan] 首页导航改为两栏
- 2026-11-20 yipan 支付回调增加幂等校验
";
    let mut src = sources(&others, memory, &[], &[]);
    src.project_name = "yipan";
    let brief = compose(&src, "robots.txt");
    assert_eq!(
        brief.memory,
        vec![
            "2026-11-20 yipan 支付回调增加幂等校验",
            "2026-11-19 [yipan] 首页导航改为两栏",
        ]
    );
}

#[test]
fn overlapping_cjk_project_names_stay_isolated() {
    let others = vec!["亿盘海外".to_owned()];
    let memory = "\
- 2026-11-16 [亿盘海外] 独立站启用了新域名
- 2026-11-17 亿盘海外 结账页汇率接口超时
- 2026-11-18 [亿盘] 国内版支持微信扫码
";
    let mut src = sources(&others, memory, &[], &[]);
    src.project_name = "亿盘";
    let brief = compose(&src, "域名");
    assert_eq!(brief.memory, vec!["2026-11-18 [亿盘] 国内版支持微信扫码"]);
}
