import { useEffect } from "react";

function shareForm(form: HTMLFormElement): boolean {
  const button = form.querySelector<HTMLButtonElement>('button[type="submit"]');
  return button?.textContent?.trim() === "Share" || button?.textContent?.trim() === "Sharing…";
}

export function SharePendingGuard() {
  useEffect(() => {
    let pendingForm: HTMLFormElement | null = null;
    let cleared = false;

    const finish = () => {
      if (pendingForm === null) {
        return;
      }
      pendingForm.querySelectorAll<HTMLSelectElement>("select").forEach((select) => {
        select.disabled = false;
      });
      const button = pendingForm.querySelector<HTMLButtonElement>('button[type="submit"]');
      if (button !== null) {
        button.disabled = false;
        button.textContent = "Share";
      }
      pendingForm.querySelector("[data-share-progress]")?.remove();
      delete pendingForm.dataset.sharePending;
      pendingForm = null;
      cleared = false;
      observer.disconnect();
    };

    const observer = new MutationObserver(() => {
      const status = document.querySelector<HTMLElement>('[role="alert"], .notice');
      const text = status?.textContent?.trim() ?? "";
      if (text === "") {
        cleared = true;
      } else if (cleared) {
        finish();
      }
    });

    const onSubmit = (event: Event) => {
      const form = event.target;
      if (!(form instanceof HTMLFormElement) || !shareForm(form)) {
        return;
      }
      if (form.dataset.sharePending === "true") {
        event.preventDefault();
        event.stopImmediatePropagation();
        return;
      }

      pendingForm = form;
      form.dataset.sharePending = "true";
      form.querySelectorAll<HTMLSelectElement>("select").forEach((select) => {
        select.disabled = true;
      });
      const button = form.querySelector<HTMLButtonElement>('button[type="submit"]');
      if (button !== null) {
        button.disabled = true;
        button.textContent = "Sharing…";
      }
      const progress = document.createElement("span");
      progress.dataset.shareProgress = "true";
      progress.className = "muted";
      progress.textContent = "Sharing…";
      form.append(progress);

      const status = document.querySelector<HTMLElement>('[role="alert"], .notice');
      const statusText = status?.textContent?.trim() ?? "";
      cleared = statusText === "";
      observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    };

    document.addEventListener("submit", onSubmit, true);
    return () => {
      document.removeEventListener("submit", onSubmit, true);
      observer.disconnect();
    };
  }, []);

  return null;
}
