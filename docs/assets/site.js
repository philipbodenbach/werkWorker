(() => {
  const root = document.documentElement;
  let theme;
  try { theme = localStorage.getItem('pb-theme'); } catch {}
  const dark = theme === 'dark' || (theme !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
  root.classList.toggle('dark', dark);
  document.addEventListener('DOMContentLoaded', () => {
    const button = document.querySelector('[data-theme-toggle]');
    if (!button) return;
    const sync = () => {
      const dark = root.classList.contains('dark');
      const label = dark ? button.dataset.lightLabel : button.dataset.darkLabel;
      button.setAttribute('aria-pressed', String(dark));
      button.setAttribute('aria-label', label);
      button.title = label;
    };
    button.addEventListener('click', () => {
      root.classList.toggle('dark');
      try { localStorage.setItem('pb-theme', root.classList.contains('dark') ? 'dark' : 'light'); } catch {}
      sync();
    });
    sync();
  });
})();
